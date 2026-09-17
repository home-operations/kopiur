//! First-class backup verification scheduling (ADR-0005 §4).
//!
//! Mirrors the `Maintenance` scheduling kernel (`crate::maintenance`): the
//! `SnapshotPolicy` reconciler is the *scheduler*. Each reconcile it decides whether
//! a quick (`kopia snapshot verify`) or deep (scratch-restore) verification is due
//! — using croner + deterministic jitter via [`crate::snapshot_schedule::next_fire`],
//! seeded by the policy UID — then spawns at most one per-slot owned mover Job and
//! tracks it to terminal. The mover evaluates the optional CEL `successExpr` and
//! PATCHes `SnapshotPolicy.status.lastVerified`.
//!
//! Hardening matches maintenance: per-slot deterministic Job names (idempotency),
//! single-flight via a label selector, a repository-ready gate (the caller already
//! gates the policy on its Repository being Ready), and `ttlSecondsAfterFinished` so
//! finished Jobs self-reap.
//!
//! The scheduling decisions ([`due_tier`], [`next_verify_wakeup`]) are **pure** and
//! unit-tested; the Job spawn is thin IO.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use k8s_openapi::api::batch::v1::Job;
use kube::api::ListParams;
use kube::{Api, ResourceExt};

use kopiur_api::common::{CronSpec, ScheduleDefaults, ScratchDefaults};
use kopiur_api::{Origin, Snapshot, SnapshotPolicy, Verification};
use kopiur_mover::workspec::{
    DeepVerify, MoverOptions, MoverWorkSpec, Operation, QuickVerify, ResolvedIdentity, TargetRef,
    VerifyOp, VerifyTier,
};

use crate::consts::{
    API_VERSION, COMPONENT_LABEL, ORIGIN_LABEL, REPOSITORY_UID_LABEL, VERIFY_COMPONENT,
    VERIFY_INSTANCE_LABEL, VERIFY_MEMBER_LABEL, VERIFY_REPO_LABEL, VERIFY_SLOT_ANNOTATION,
};
use crate::context::Context;
use crate::error::Result;
use crate::io::{self, ResolvedRepository};
use crate::jobs::{
    self, CacheVolume, DEEP_SCRATCH_PATH, JobLimits, MoverJobInputs, VolumeMountSpec,
};
use crate::naming::short_hash;
use crate::snapshot::{backend_to_repository_connect, job_terminal_state};
use crate::snapshot_schedule::{next_fire, parse_go_duration};

/// How long a finished verify Job lingers before TTL-reaping.
const VERIFY_JOB_TTL_SECS: i64 = 3600;
/// Requeue while a verify Job is in flight.
const REQUEUE_RUNNING: Duration = Duration::from_secs(30);
/// Requeue after a failed verify Job (re-check / bounded retry once TTL-reaped).
const REQUEUE_FAILED: Duration = Duration::from_secs(300);
/// Requeue while verification is gated (no verifiable snapshot yet, #168). The
/// steady policy cadence — deliberately NOT [`REQUEUE_RUNNING`]'s 30s hot loop,
/// which [`next_verify_wakeup`] would otherwise produce for the past-due catch-up
/// slot. The first successful backup re-reconciles the policy promptly via its
/// child-Snapshot watch, so this never delays the first real verify.
const REQUEUE_GATED: Duration = Duration::from_secs(300);
/// Upper bound on any requeue so the schedule is re-evaluated within the heartbeat.
const REQUEUE_CAP: Duration = Duration::from_secs(1800);

/// Which verification tier to run, mirroring `MaintenanceMode`. Deep subsumes quick
/// (a deep restore-test is the stronger proof), so when both are due, deep wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyTierKind {
    /// Blob-level `kopia snapshot verify`.
    Quick,
    /// Scratch-restore restorability test.
    Deep,
}

impl VerifyTierKind {
    /// Stable one-char tag for the per-slot Job name.
    fn tag(self) -> &'static str {
        match self {
            VerifyTierKind::Quick => "q",
            VerifyTierKind::Deep => "d",
        }
    }
}

/// The instant after which to search for `tier`'s next slot: its last run (from
/// `status.lastVerified` for the freshest known verify) or a year ago so the first
/// reconcile fires. Both tiers share `lastVerified` (a single surfaced timestamp),
/// which is conservative: a just-run quick verify briefly defers a due deep one,
/// re-evaluated on the next reconcile.
fn tier_after(last_verified: Option<DateTime<Utc>>) -> DateTime<Utc> {
    last_verified.unwrap_or_else(|| Utc::now() - chrono::Duration::days(365))
}

/// The next cron slot for a verification `CronSpec` strictly after `after`, seeded by
/// the policy UID for a stable per-replica spread and evaluated in the cron's own
/// `timezone` (absent ⇒ the repository's `scheduleDefaults.timezone`, else UTC),
/// spread by the cron's own `jitter` (absent ⇒ the repository's
/// `scheduleDefaults.jitter`, else no jitter). Both tiers (quick and deep) run
/// through here, so both inherit.
fn slot_for(
    seed: &str,
    spec: &CronSpec,
    after: DateTime<Utc>,
    repo_defaults: Option<&ScheduleDefaults>,
) -> Result<DateTime<Utc>> {
    let jitter = kopiur_api::common::effective_jitter(
        spec.jitter.as_deref(),
        repo_defaults.and_then(|d| d.jitter.as_deref()),
    )
    .as_deref()
    .and_then(parse_go_duration);
    let tz = kopiur_api::common::resolve_tz_with_default(
        spec.timezone.as_deref(),
        repo_defaults.and_then(|d| d.timezone.as_deref()),
    );
    next_fire(&spec.cron, jitter, seed, after, tz)
}

/// Whether verification is *unlocked*: a snapshot actually exists to verify. Pure.
///
/// The `tier_after` catch-up (a "never verified" policy anchors a year ago so the
/// first slot is immediately past-due) is deliberate for missed slots — but on a
/// brand-new policy it would fire a verify Job *before any backup exists*, and the
/// mover fails hard (`deep verify found no snapshot to restore …`, #168). Gate it:
/// schedule no verify until either this policy has produced a successful backup
/// covering THIS member's path (`snapshot_policy::member_has_success`)
/// OR its resolved repository already contains a **discovered** (adopted) or
/// **replicated** row AT THIS MEMBER'S IDENTITY
/// (`discovered_identities`) — the adopted-repo escape hatch, where deep
/// verify legitimately resolves the latest repo snapshot for the identity.
///
/// Both inputs are per (repository x member). An earlier shape ORed in "this
/// repository holds SOME discovered/replicated row", which is the ordinary state
/// of a new policy on a shared repository (see
/// `AdoptionSummary::last_scan_unmatched`): it unlocked verification for an
/// identity with no manifest at all, and since a quick verify that covered
/// nothing is now terminal, that meant a FAILED verify Job every slot until the
/// policy's first own backup landed.
pub fn verification_unlocked(has_successful: bool, has_discovered: bool) -> bool {
    has_successful || has_discovered
}

/// Where to look for a repository's **discovered** (adopted) `Snapshot` CRs when
/// probing the #168 escape hatch. Derived purely from whether the policy's resolved
/// repository is namespaced or cluster-scoped — the two placements the catalog uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveredProbeScope {
    /// Namespaced [`Repository`](kopiur_api::Repository): the catalog materializes
    /// discovered rows in a single namespace ([`crate::catalog::Placement::Namespace`]),
    /// so a namespaced LIST there suffices.
    Namespace(String),
    /// Cluster-scoped [`ClusterRepository`](kopiur_api::ClusterRepository): the catalog
    /// scatters discovered rows across identity-hostname namespaces / the
    /// `fallbackNamespace` ([`crate::catalog::Placement::Cluster`]) — generally NOT the
    /// policy's namespace — so probe cluster-wide. The `repository-uid` label already
    /// scopes the LIST to this repository's rows, so the wide scan can't false-positive
    /// on another repo's discovered snapshots.
    ClusterWide,
}

/// **Pure.** Decide where [`discovered_identities`] should LIST, from the
/// resolved repository's own namespace ([`ResolvedRepository::repo_namespace`]).
///
/// Namespaced repos probe **their own** namespace, not the probing policy's: the
/// catalog materializes discovered rows in the repository's namespace
/// ([`crate::catalog::Placement::Namespace`]), and `RepositoryRef` allows a policy to
/// reference a `Repository` in a different namespace from its own
/// (`kopiur_api::validate::repository`) — so a same-namespace assumption silently
/// never unlocks verification for a cross-namespace-referenced adopted repository. A
/// `ClusterRepository` (`repo_namespace: None`) scatters its rows across arbitrary,
/// per-identity namespaces the reconciler can't enumerate a priori, so the only
/// correct probe there is cluster-wide — otherwise an *adopted* `ClusterRepository`
/// would never unlock verification before its first child backup (#168 regression).
pub fn discovered_probe_scope(repo_namespace: Option<&str>) -> DiscoveredProbeScope {
    match repo_namespace {
        Some(ns) => DiscoveredProbeScope::Namespace(ns.to_string()),
        None => DiscoveredProbeScope::ClusterWide,
    }
}

// --- #456: the (repository x member) verify grid -----------------------------

/// The stable 6-hex per-member tag: [`crate::naming::short_hash`] over the
/// member's DERIVED kopia source path, truncated to 6 — the same budget
/// reasoning as [`crate::naming::repo_tag6`], and label-safe where the raw
/// `/pvc/<name>` path (slashes) is not. Keyed on the PATH rather than the PVC
/// name so it is stable under `sourcePathStrategy` (two same-named PVCs in
/// different namespaces get different tags exactly when they get different
/// paths).
pub fn member_tag6(source_path: &str) -> String {
    crate::naming::short_hash(source_path)
        .chars()
        .take(6)
        .collect()
}

/// One member of a policy's verification fan-out (#456): the single kopia
/// source path one verify run covers.
///
/// A `pvcSelector` policy has **N** of these — one per matched PVC — because
/// the backup side minted one kopia source per matched PVC. Verifying such a
/// policy as ONE run resolved a PATHLESS identity (`user@host:`), which kopia's
/// `ParseSourceInfo` then treats as a relative filesystem path: deep verify
/// failed with `deep verify found no snapshot to restore for source path ""`
/// and, worse, **quick verify matched zero manifests and exited 0** — a silent
/// false pass that still stamped `lastVerified`. That is #456.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyMember {
    /// The DERIVED kopia source path this member verifies, passed to the
    /// identity kernel as a `sourcePathOverride` so it is used VERBATIM — the
    /// same thing the backup did when it wrote the snapshot.
    ///
    /// `None` means "derive the path the way the pre-#456 verify did": the
    /// governing source's own `pvc`/`nfs`/`sourcePathOverride`. That is the
    /// only shape a non-selector policy ever takes, and it keeps a zero-source
    /// legacy policy's PATHLESS identity byte-for-byte.
    pub source_path: Option<String>,
    /// The stable 6-hex member tag ([`member_tag6`]), `Some` for **every**
    /// member of a `pvcSelector` policy — including a selector that currently
    /// matches exactly ONE PVC — and `None` only for a non-selector policy.
    ///
    /// A one-member selector policy is deliberately NOT collapsed to `None`.
    /// Its byte-identity to a pre-#456 operator is already gone (a different
    /// identity source path is the entire fix), and keeping it in flat mode
    /// would leave the pre-#456 false-pass `status.lastVerified` in place as
    /// both a displayed lie AND, via `verify_cell_anchor`, the anchor that
    /// defers the first REAL verification by up to a full period of the
    /// coarsest due tier — a month, for a monthly deep drill. Stamps-map mode
    /// gets that value cleared on the first reconcile with no new state, and it
    /// also means growing the selector 1 -> 2 no longer discards the verified
    /// member's real result. Job-name continuity is moot here: the legacy
    /// unlabelled in-flight Job blocks the new name anyway
    /// ([`job_blocks_cell`]).
    pub member6: Option<String>,
}

/// **Pure.** The verification members of `policy`, mirroring EXACTLY what the
/// backup side mints.
///
/// The mirror is [`kopiur_api::expand::expand_sources`]: a policy that mixes a
/// plain `pvc:` source with a `pvcSelector` source backs up **only the selector
/// members** (`expand_sources` skips non-selector sources once any selector
/// exists), so verification must too. Returning the union instead would derive
/// a path the repository has no snapshot for and fail the whole policy.
///
/// Built on `EffectiveSource::kopia_source_path` + `strategy_for` — the
/// same two functions `expand_sources` uses — and deliberately NOT on
/// [`kopiur_api::expand::ExpandedMember::name`]: that name carries a 63-char
/// bound that can error, and verification mints no child CRs, so it needs no
/// name.
///
/// * no selector source ⇒ exactly one member, `source_path: None`,
///   `member6: None` (the pre-#456 identity, verbatim);
/// * a selector matching nothing ⇒ an EMPTY Vec, mirroring
///   `SlotMintPlan::NothingMatched`. The caller warns, spawns nothing and
///   stamps nothing — never a pathless run that false-passes;
/// * a selector matching exactly ONE PVC still gets a `member6`. See
///   [`VerifyMember::member6`] for why that shape is not collapsed.
pub fn verify_members(
    policy: &SnapshotPolicy,
    matched: &BTreeMap<usize, Vec<kopiur_api::snapshot::PvcTargetRef>>,
) -> Vec<VerifyMember> {
    use kopiur_api::expand::{EffectiveSource, strategy_for};
    if !policy.spec.sources.iter().any(|s| s.pvc_selector.is_some()) {
        return vec![VerifyMember {
            source_path: None,
            member6: None,
        }];
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut members: Vec<VerifyMember> = Vec::new();
    for (index, source) in policy.spec.sources.iter().enumerate() {
        if source.pvc_selector.is_none() {
            continue;
        }
        let strategy = strategy_for(source);
        for target in matched.get(&index).into_iter().flatten() {
            let eff = EffectiveSource {
                index,
                pvc: Some(target.clone()),
                nfs_path: None,
                source_path_override: source.source_path_override.clone(),
                read_only: kopiur_api::snapshot_policy::source_read_only(source),
            };
            // `/data` is not a guess, it is the byte-for-byte mirror of
            // `expand_sources`' own fallback for this case, so verification
            // looks exactly where the backup wrote. A silent `continue` here
            // would drop a member from a COVERAGE calculation, which is the
            // wrong default even for a branch a PVC-bearing `EffectiveSource`
            // cannot currently reach.
            let path = eff
                .kopia_source_path(strategy)
                .unwrap_or_else(|| "/data".to_string());
            // Two selector sources landing on one path is a configuration error
            // `expand_sources` REFUSES; verification only ever reads, so it
            // degrades to verifying that one path once rather than parking the
            // policy on a fault the backup side already reports.
            if !seen.insert(path.clone()) {
                continue;
            }
            members.push(VerifyMember {
                member6: Some(member_tag6(&path)),
                source_path: Some(path),
            });
        }
    }
    members
}

/// Thin IO over [`verify_members`]: one `match_pvcs` LIST per **reconcile**
/// (never per repository — `verify_step` runs once per repository and would
/// otherwise multiply the LIST by the repository count).
///
/// The LIST is skipped entirely for a policy with no `pvcSelector` source:
/// `crate::expand::match_pvcs` iterates sources and `continue`s past every
/// non-selector one, so it performs no cluster IO there.
pub async fn resolve_verify_members(
    ctx: &Context,
    policy: &SnapshotPolicy,
) -> Result<Vec<VerifyMember>> {
    let matched = crate::expand::match_pvcs(&ctx.client, policy).await?;
    Ok(verify_members(policy, &matched))
}

/// How many `Snapshot` rows one page of the #168 discovered-identity probe
/// reads. Only the identity triples are retained, so this bounds the request
/// size, not the result.
const DISCOVERED_PROBE_PAGE_SIZE: u32 = 500;

/// A kopia identity reduced to its comparable triple —
/// `(username, hostname, sourcePath)` — for the #168 discovered-row probe.
///
/// Same equality rule as adoption's own structural match: all three components
/// must be equal, and `None` is deliberately DISTINCT from `Some("")` (a
/// pathless identity is a different kopia address from one with an empty path).
pub type IdentityKey = (String, String, Option<String>);

/// **Pure.** See [`IdentityKey`].
pub fn identity_key(id: &kopiur_api::common::ResolvedIdentity) -> IdentityKey {
    (
        id.username.clone(),
        id.hostname.clone(),
        id.source_path.clone(),
    )
}

/// **Pure.** Which identities the #168 catalog probe is worth asking about:
/// the still-GATED members' only, given `(has_successful, identity key)` per
/// member.
///
/// An EMPTY result means skip the read entirely — once every member has a
/// Succeeded child covering its own path, no adopted row can change any
/// outcome, so the paged LIST is not worth a request. Handing the wanted set to
/// [`discovered_identities`] also lets it drop every irrelevant row and stop as
/// soon as all of them are found, which collapses the common case to one page.
pub fn probe_identities(cells: &[(bool, IdentityKey)]) -> BTreeSet<IdentityKey> {
    cells
        .iter()
        .filter(|(has_successful, _)| !has_successful)
        .map(|(_, key)| key.clone())
        .collect()
}

/// **Pure.** The `status.verificationStamps` key one (repository, member) cell
/// stamps, or `None` for the classic flat `status.lastVerified` write.
///
/// `#` is the member separator because it cannot occur in a repo key
/// ([`kopiur_api::common::repo_key`] is `Kind/ns/name`), which makes
/// [`parse_stamp_key`] total and unambiguous. A member-only key (a single-repo
/// policy that fans out) is therefore `#<member6>` — an empty repo segment.
pub fn stamp_key(repo_key: Option<&str>, member6: Option<&str>) -> Option<String> {
    // The load-bearing invariant of the whole prune. If a repo key ever
    // admitted '#', `parse_stamp_key` would split it in the wrong place, the
    // repo segment would not match any current key, and `stamp_key_live` would
    // delete a LIVE stamp every reconcile — `lastVerified` would silently never
    // advance and a due deep verify would re-fire forever. Asserted over real
    // `repo_key` output in `tests::repo_keys_are_hash_free_so_the_separator_is_unambiguous`.
    debug_assert!(
        repo_key.is_none_or(|r| !r.contains('#')),
        "repository key {repo_key:?} contains the '#' member separator"
    );
    match (repo_key, member6) {
        (None, None) => None,
        (Some(r), None) => Some(r.to_string()),
        (r, Some(m)) => Some(format!("{}#{m}", r.unwrap_or(""))),
    }
}

/// **Pure and total.** Split a persisted stamp key into its
/// `(repository segment, member tag)`. A key with no `#` is repo-only (the
/// #368 shape); an empty repository segment is a single-repo fan-out.
pub fn parse_stamp_key(key: &str) -> (&str, Option<&str>) {
    match key.rsplit_once('#') {
        Some((repo, member)) => (repo, Some(member)),
        None => (key, None),
    }
}

/// **Pure.** Whether a persisted `verificationStamps` key still describes a
/// LIVE cell, i.e. must survive the prune.
///
/// `repo_keys` carries the empty string for a single-repo policy (the
/// repo-agnostic segment its keys use). `member6s` is empty when the policy
/// does not fan out.
///
/// This replaces the #368 prune, which kept a key only when it equalled a
/// current repo key **exactly** — so it deleted every member-keyed stamp on the
/// very next reconcile (and, for a single-repo policy, nulled the whole map
/// every pass). With the stamps gone the fold could never advance
/// `lastVerified`, and a due deep verify re-fired every slot forever.
pub fn stamp_key_live(key: &str, repo_keys: &BTreeSet<&str>, member6s: &BTreeSet<&str>) -> bool {
    let (repo, member) = parse_stamp_key(key);
    repo_keys.contains(repo)
        && match member {
            Some(m) => member6s.contains(m),
            // A bare repo key is live only while there is no member dimension:
            // once the policy fans out, the pre-fan-out stamp described a
            // PATHLESS run that false-passed and must not anchor anything.
            None => member6s.is_empty(),
        }
}

/// Decide which verification tier is due now, preferring deep (it subsumes quick).
/// Returns the tier + its scheduled slot, or `None` if nothing is due. Pure given
/// the policy's `verification`, the seed, the last-verified time, `now`, the
/// repository's `scheduleDefaults` (`repo_defaults`, GitHub #174 item 3), and
/// whether verification is `unlocked` ([`verification_unlocked`]) — a locked policy
/// is never due (the #168 gate), so the catch-up slot is not consumed until a
/// snapshot exists.
pub fn due_tier(
    verification: &Verification,
    seed: &str,
    last_verified: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    repo_defaults: Option<&ScheduleDefaults>,
    unlocked: bool,
) -> Option<(VerifyTierKind, DateTime<Utc>)> {
    if !unlocked {
        return None;
    }
    let after = tier_after(last_verified);
    if let Some(d) = &verification.deep
        && let Ok(slot) = slot_for(seed, &d.schedule, after, repo_defaults)
        && now >= slot
    {
        return Some((VerifyTierKind::Deep, slot));
    }
    // `quick.schedule == None` ⇒ quick tier disabled. This is the old-shape
    // decode-tolerance path: a stale persisted `quick: {cron: ...}` decodes as
    // `schedule: None`, so the quick tier is simply not due (never panics/wedges);
    // new writes with the old shape are rejected at admission.
    if let Some(q) = &verification.quick
        && let Some(schedule) = &q.schedule
        && let Ok(slot) = slot_for(seed, schedule, after, repo_defaults)
        && now >= slot
    {
        return Some((VerifyTierKind::Quick, slot));
    }
    None
}

/// How long until the next verification slot (either tier), measured from
/// `last_verified`. Floored at the running cadence, capped by `REQUEUE_CAP`.
pub fn next_verify_wakeup(
    verification: &Verification,
    seed: &str,
    last_verified: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    repo_defaults: Option<&ScheduleDefaults>,
) -> Duration {
    let after = tier_after(last_verified);
    let mut earliest: Option<DateTime<Utc>> = None;
    for spec in [
        verification
            .quick
            .as_ref()
            .and_then(|q| q.schedule.as_ref()),
        verification.deep.as_ref().map(|d| &d.schedule),
    ]
    .into_iter()
    .flatten()
    {
        if let Ok(slot) = slot_for(seed, spec, after, repo_defaults) {
            earliest = Some(earliest.map_or(slot, |e| e.min(slot)));
        }
    }
    match earliest {
        // A future slot: sleep until it (floored/capped).
        Some(slot) if slot > now => (slot - now)
            .to_std()
            .unwrap_or(REQUEUE_CAP)
            .min(REQUEUE_CAP)
            .max(REQUEUE_RUNNING),
        // A past-due slot: the prompt 30s cadence, since the caller then spawns and
        // tracks the Job.
        Some(_) => REQUEUE_RUNNING,
        // No live schedule at all (e.g. an old-shape quick-only policy whose
        // `quick.schedule` decoded to `None`, with no deep): nothing will ever come
        // due, so floor at the steady cadence rather than the 30s hot loop.
        None => REQUEUE_GATED,
    }
}

/// The requeue delay when no verify tier is due, honoring the #168 gate. A gated
/// policy (`!unlocked`) requeues at the steady cadence ([`REQUEUE_GATED`]) — never
/// [`next_verify_wakeup`]'s past-due 30s hot loop — since it is re-triggered by its
/// first successful child Snapshot. An unlocked policy sleeps until its next slot
/// (capped by [`REQUEUE_CAP`]). Pure so the gated-vs-scheduled requeue is testable.
fn idle_requeue(
    verification: &Verification,
    seed: &str,
    last_verified: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    repo_defaults: Option<&ScheduleDefaults>,
    unlocked: bool,
) -> Duration {
    if unlocked {
        next_verify_wakeup(verification, seed, last_verified, now, repo_defaults).min(REQUEUE_CAP)
    } else {
        REQUEUE_GATED
    }
}

/// Deterministic, ≤52-char, DNS-1123-safe per-slot verify Job name:
/// `<policy>-vfy-<q|d>-<unix_slot>` (truncate+hash long policy names, like
/// maintenance).
///
/// `repo6` is the 6-hex per-repository tag ([`crate::naming::repo_tag6`]),
/// present ONLY for a multi-repository policy: `<policy>-vfy-<q|d>-<r6>-<unix>`.
/// `member6` is the 6-hex per-member tag ([`member_tag6`]), present for every
/// member of a `pvcSelector` policy (#456) — a one-match selector included:
/// `<policy>-vfy-<q|d>[-<r6>]-m<m6>-<unix>`. Both are spliced BETWEEN the tier
/// and the unix slot, so the single-repo non-selector shape (both `None`) stays
/// byte-identical to every prior operator and an in-flight slot's Job is still
/// found by name across an upgrade (slot continuity — a renamed slot would
/// double-spawn).
///
/// `MAX` is **52, not 63**: the remaining 11 bytes of the 63-byte label budget
/// are reserved for the `-<5-char>` pod-name suffix Kubernetes appends.
fn verify_job_name(
    policy: &str,
    tier: VerifyTierKind,
    slot: DateTime<Utc>,
    repo6: Option<&str>,
    member6: Option<&str>,
) -> String {
    const MAX: usize = 52;
    let repo_seg = repo6.map(|r6| format!("-{r6}")).unwrap_or_default();
    let member_seg = member6.map(|m6| format!("-m{m6}")).unwrap_or_default();
    let suffix = format!(
        "-vfy-{}{repo_seg}{member_seg}-{}",
        tier.tag(),
        slot.timestamp()
    );
    let budget = MAX.saturating_sub(suffix.len());
    if policy.len() <= budget {
        format!("{policy}{suffix}")
    } else {
        let hash = short_hash(policy);
        let keep = budget.saturating_sub(hash.len() + 1);
        let trunc: String = policy.chars().take(keep).collect();
        format!("{trunc}-{hash}{suffix}")
    }
}

/// One repository's verify-scheduling inputs, prepared by the `SnapshotPolicy`
/// reconciler per target. The single-repo shape passes exactly one with
/// `repo_key: None` (byte-identical behavior — flat `status.lastVerified`
/// anchor, un-suffixed Job names, mover stamps the flat field); a multi-repo
/// policy passes one per READY repository with `repo_key: Some(_)`.
pub struct VerifyTarget<'a> {
    /// The spec's ref for this repository, as written (creds naming + resolution).
    pub rref: &'a kopiur_api::common::RepositoryRef,
    /// The resolved repository surface.
    pub repo: &'a ResolvedRepository,
    /// `Some(normalized repo key)` for a multi-repo policy — flows into the
    /// Job name's `<r6>` segment, the [`VERIFY_REPO_LABEL`] single-flight
    /// scope, and the work spec (so the mover stamps the entry-keyed
    /// `verificationStamps[<key>]` instead of the flat field). `None` =
    /// single-repo.
    pub repo_key: Option<String>,
    /// This target's last-verified anchor: the flat `status.lastVerified` for
    /// single-repo; the folded per-repo entry for multi (each repository
    /// catches up on its own schedule, so a fresh repo B is immediately due
    /// without repo A's recent verify deferring it).
    pub last_verified: Option<DateTime<Utc>>,
    /// Whether THIS repository holds a verifiable snapshot from this policy
    /// (#168 gate input).
    pub has_successful: bool,
    /// The DERIVED kopia source path this cell verifies (#456), passed to the
    /// identity kernel verbatim. `None` reproduces the pre-#456 identity: the
    /// governing source's own `pvc`/`nfs`/`sourcePathOverride`, and a PATHLESS
    /// identity for a zero-source legacy policy.
    pub source_path: Option<&'a str>,
    /// `Some(6-hex member tag)` for every member of a `pvcSelector` policy
    /// (#456) — including a selector currently matching exactly ONE PVC, which
    /// is deliberately not collapsed; `None` only for a non-selector source.
    /// See [`VerifyMember::member6`].
    ///
    /// Orthogonal to [`Self::repo_key`]: the repo dimension
    /// drives `repo_tag6`, the [`VERIFY_REPO_LABEL`] value and the projected
    /// credentials prefix (members under one (policy, repository) SHARE that
    /// Secret), while this drives only the `-m<6>` Job-name segment, the
    /// [`VERIFY_MEMBER_LABEL`] value and the stamp key's member segment.
    pub member6: Option<&'a str>,
}

/// What one (repository x member) verify cell asked of the reconciler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyStepResult {
    /// The requeue this cell wants, or `None` when `spec.verification` is
    /// absent (no behavior change — the caller keeps its steady cadence).
    pub requeue: Option<Duration>,
    /// A **deep**-tier Job for this cell is in flight (just spawned, or still
    /// running). The caller must not start another member's deep Job in the
    /// same reconcile: deep members run SEQUENTIALLY because each provisions
    /// its own `deep.capacity` of scratch, so a 4-PVC policy at `500Gi` would
    /// otherwise ask the cluster for 2 TiB of ephemeral storage at once.
    /// Correctness is never at stake (the scratch volume is per-Job — an
    /// `Ephemeral` PVC or an `emptyDir`, never a shared one); this is a cost
    /// guard.
    pub deep_active: bool,
}

impl VerifyStepResult {
    /// The "verification is not configured" answer.
    fn unconfigured() -> Self {
        Self {
            requeue: None,
            deep_active: false,
        }
    }
    /// A requeue with no deep Job in flight.
    fn requeue(d: Duration) -> Self {
        Self {
            requeue: Some(d),
            deep_active: false,
        }
    }
}

/// The verification scheduling step, called by the `SnapshotPolicy` reconciler
/// (after it has confirmed the policy is non-suspended and this target's
/// repository is Ready) — once per (READY repository x verification member)
/// cell.
///
/// `deep_hold` is the sequential-deep guard: `true` once another member's deep
/// Job is in flight this reconcile, which defers this cell's deep tier (see
/// [`VerifyStepResult::deep_active`]).
///
/// `has_discovered` is the #168 escape-hatch probe result, hoisted to
/// per-TARGET by the caller so the LIST is not repeated per member.
pub async fn verify_step(
    config: &SnapshotPolicy,
    ctx: &Context,
    target: &VerifyTarget<'_>,
    namespace: &str,
    has_discovered: bool,
    deep_hold: bool,
) -> Result<VerifyStepResult> {
    let Some(verification) = config.spec.verification.as_ref() else {
        return Ok(VerifyStepResult::unconfigured());
    };
    let repo = target.repo;
    let has_successful = target.has_successful;
    let name = config.name_any();
    // #456: a fanned-out policy seeds each member's cron spread by
    // `<uid>|<path>`, so N members do not all fire in the same second (N
    // concurrent quick verifies, or a stampede of deep ones). A one-member
    // policy keeps the bare UID seed, so its slots are byte-identical.
    let seed = match target.member6 {
        None => config.uid().unwrap_or_else(|| name.clone()),
        Some(_) => format!(
            "{}|{}",
            config.uid().unwrap_or_else(|| name.clone()),
            target.source_path.unwrap_or_default()
        ),
    };
    let now = Utc::now();
    let last_verified = target.last_verified;
    let repo6 = target.repo_key.as_deref().map(crate::naming::repo_tag6);
    // GitHub #174 item 3, both scheduling inputs: a per-cron `timezone` wins, else
    // the repository's `scheduleDefaults.timezone`, else UTC; and a per-cron
    // `jitter` wins, else `scheduleDefaults.jitter`, else no jitter.
    let repo_defaults = repo.schedule_defaults.as_ref();

    // #168 gate: never schedule a verify before a verifiable snapshot exists — the
    // tier_after catch-up would otherwise fire a Job on the first reconcile, before
    // any backup, and the mover fails hard. Unlocked once this policy has a Succeeded
    // snapshot OR its repo already carries discovered (adopted) or replicated
    // snapshots (a replication DEST repo holding only copies is not empty). The
    // probe itself ([`discovered_identities`]) is hoisted to per-target by
    // the caller — it is a per-REPOSITORY fact, so repeating it per member would
    // multiply the LIST by the member count for no new information.
    let unlocked = verification_unlocked(has_successful, has_discovered);

    let Some((tier, slot)) = due_tier(
        verification,
        &seed,
        last_verified,
        now,
        repo_defaults,
        unlocked,
    ) else {
        if !unlocked {
            tracing::debug!(
                policy = %name,
                "verification gated: no verifiable snapshot yet (no successful backup and no \
                 discovered snapshots); deferring until the first successful backup"
            );
        }
        return Ok(VerifyStepResult::requeue(idle_requeue(
            verification,
            &seed,
            last_verified,
            now,
            repo_defaults,
            unlocked,
        )));
    };

    // Sequential deep tier: another member's deep Job is already running this
    // reconcile, so hold this one rather than provisioning a second scratch
    // volume of `deep.capacity` alongside it. Re-evaluated on the prompt
    // running cadence, so the held member starts as soon as the first finishes.
    if tier == VerifyTierKind::Deep && deep_hold {
        tracing::debug!(
            policy = %name,
            member = target.member6.unwrap_or("<single>"),
            "deferring deep verification: another member's deep Job is in flight (deep \
             members run sequentially so N members do not provision N scratch volumes)"
        );
        return Ok(VerifyStepResult {
            requeue: Some(REQUEUE_RUNNING),
            deep_active: true,
        });
    }

    let job_name = verify_job_name(&name, tier, slot, repo6.as_deref(), target.member6);
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    match job_api.get_opt(&job_name).await? {
        Some(job) => match job_terminal_state(&job) {
            // Success: the mover stamped lastVerified; sleep until the next slot.
            // The terminal Job (its env carries the whole run spec) self-reaps
            // via its TTL — it is the slot-handled marker until then.
            Some(true) => Ok(VerifyStepResult::requeue(
                next_verify_wakeup(verification, &seed, Some(now), now, repo_defaults)
                    .min(REQUEUE_CAP),
            )),
            // Failure: the failed Job lingers to its TTL as the bounded-retry
            // backoff (and keeps the pod logs).
            Some(false) => Ok(VerifyStepResult::requeue(REQUEUE_FAILED)),
            None => Ok(VerifyStepResult {
                requeue: Some(REQUEUE_RUNNING),
                deep_active: tier == VerifyTierKind::Deep,
            }),
        },
        None => {
            spawn_if_slot_free(
                config,
                ctx,
                target,
                namespace,
                &job_api,
                (&name, &job_name),
                verification,
                tier,
                slot,
                repo6.as_deref(),
            )
            .await
        }
    }
}

/// The single-flight check + spawn for a slot that has no Job yet — the
/// `None` arm of [`verify_step`]'s per-slot lookup, extracted so `verify_step`
/// stays under the cognitive-complexity ratchet.
#[allow(clippy::too_many_arguments)]
async fn spawn_if_slot_free(
    config: &SnapshotPolicy,
    ctx: &Context,
    target: &VerifyTarget<'_>,
    namespace: &str,
    job_api: &Api<Job>,
    names: (&str, &str),
    verification: &Verification,
    tier: VerifyTierKind,
    slot: DateTime<Utc>,
    repo6: Option<&str>,
) -> Result<VerifyStepResult> {
    let (policy_name, job_name) = names;
    let held = VerifyStepResult {
        requeue: Some(REQUEUE_RUNNING),
        deep_active: tier == VerifyTierKind::Deep,
    };
    // The deep tier's single-flight is member-AGNOSTIC but tier-SCOPED
    // ([`job_blocks_cell`]): that is what makes deep members sequential ACROSS
    // reconciles and operator restarts rather than only within one pass, while
    // an unrelated member's short QUICK run never defers the drill.
    if has_active_verify_job(job_api, policy_name, repo6, tier, target.member6).await? {
        return Ok(held);
    }
    spawn_verify_job(
        config,
        ctx,
        target,
        namespace,
        policy_name,
        job_name,
        verification,
        tier,
        slot,
    )
    .await?;
    tracing::info!(
        policy = %policy_name,
        ?tier,
        repo = target.repo_key.as_deref().unwrap_or("<single>"),
        member = target.member6.unwrap_or("<single>"),
        source_path = target.source_path.unwrap_or("<identity-derived>"),
        slot = %slot.to_rfc3339(),
        "spawned verification Job"
    );
    Ok(held)
}

/// Build + apply the per-slot verification mover Job.
#[allow(clippy::too_many_arguments)]
async fn spawn_verify_job(
    config: &SnapshotPolicy,
    ctx: &Context,
    target: &VerifyTarget<'_>,
    namespace: &str,
    policy_name: &str,
    job_name: &str,
    verification: &Verification,
    tier: VerifyTierKind,
    slot: DateTime<Utc>,
) -> Result<()> {
    let repo = target.repo;
    let work_spec = build_verify_work_spec(
        config,
        repo,
        namespace,
        policy_name,
        verification,
        tier,
        target.repo_key.clone(),
        target.source_path,
        stamp_key(target.repo_key.as_deref(), target.member6),
    )?;

    let mut labels = BTreeMap::new();
    labels.insert(COMPONENT_LABEL.to_string(), VERIFY_COMPONENT.to_string());
    labels.insert(VERIFY_INSTANCE_LABEL.to_string(), policy_name.to_string());
    // #456 fan-out: label the member so the per-member single-flight can tell
    // sibling members apart client-side. Present for every `pvcSelector`
    // member (a one-match selector included); absent only for a non-selector
    // source, whose label set therefore stays byte-identical.
    if let Some(m6) = target.member6 {
        labels.insert(VERIFY_MEMBER_LABEL.to_string(), m6.to_string());
    }
    // Multi-repo: scope the single-flight selector per (policy, repository) so
    // the N per-repo verifies run concurrently while each repository still gets
    // at most one Job. Single-repo Jobs deliberately stay unlabeled, matching
    // in-flight Jobs from older operators.
    if let Some(r6) = target.repo_key.as_deref().map(crate::naming::repo_tag6) {
        labels.insert(VERIFY_REPO_LABEL.to_string(), r6);
    }
    let mut annotations = BTreeMap::new();
    annotations.insert(VERIFY_SLOT_ANNOTATION.to_string(), slot.to_rfc3339());

    // Filesystem repos need the repo volume mounted; object stores reach the backend
    // over the network. Mounted read-write (verify reads, deep restore writes scratch).
    let repo_volume =
        io::filesystem_repo_mount_source(&repo.backend).map(|source| VolumeMountSpec {
            source,
            mount_path: io::filesystem_repo_path(&repo.backend).unwrap_or_default(),
            read_only: false,
        });
    let owner = io::owner_ref_for(config, "SnapshotPolicy")?;

    let mover_identity = io::ensure_mover_identity(
        &ctx.client,
        namespace,
        &[&repo.backend],
        ctx.mover_service_account.as_deref(),
        ctx.mover_role_kind.as_str(),
        &ctx.mover_clusterrole,
    )
    .await?;
    mover_identity.decorate_labels(&mut labels);

    // Per-repo creds prefix for multi (`<policy>-vfy-<r6>`): N concurrent
    // per-repo verifies must never share a projected-Secret name for different
    // repositories. Single-repo keeps the classic `<policy>-vfy` prefix so
    // projected Secret names stay byte-identical.
    let creds_prefix = match target.repo_key.as_deref().map(crate::naming::repo_tag6) {
        Some(r6) => io::CredsPrefix::verification_for_repo(policy_name, &r6),
        None => io::CredsPrefix::verification(policy_name),
    };
    let creds = io::resolve_mover_creds_for(
        &ctx.client,
        namespace,
        &creds_prefix,
        &owner,
        repo,
        config
            .spec
            .credential_projection
            .as_ref()
            .is_some_and(|p| p.enabled),
        io::repo_kind_str(target.rref.kind),
        &target.rref.name,
    )
    .await?;
    if creds.projected > 0 {
        ctx.metrics
            .inc_secrets_projected(namespace, creds.projected);
    }
    let creds_secrets = io::plain_creds(creds.names);

    // The verify mover inherits the repository's moverDefaults (security context,
    // placement, resources, TTL, cache) merged under the recipe's mover (ADR-0004
    // §1/§2). The cache *volume* is resolved separately below (verify never attaches
    // the backup's persistent cache PVC); the cache *budgets* ride the work-spec.
    let resolved_mover = kopiur_api::common::resolve_mover(
        repo.mover_defaults.as_ref(),
        config
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.security_context.as_ref()),
        config
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.pod_security_context.as_ref()),
        config
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.resources.as_ref()),
        config.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
        config
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.ttl_seconds_after_finished),
    );
    let limits = JobLimits {
        ttl_seconds_after_finished: resolved_mover
            .ttl_seconds_after_finished
            .or(Some(VERIFY_JOB_TTL_SECS)),
        ..JobLimits::default()
    };

    let inputs = MoverJobInputs {
        name: job_name,
        namespace,
        owner,
        work_spec: &work_spec,
        image: &ctx.mover_image,
        image_pull_policy: ctx.mover_pull_policy(),
        limits,
        resources: resolved_mover.resources.clone(),
        security_context: resolved_mover.security_context.clone(),
        pod_security_context: resolved_mover.pod_security_context.clone(),
        node_selector: resolved_mover.node_selector.clone(),
        tolerations: resolved_mover.tolerations.clone(),
        affinity: resolved_mover.affinity.clone(),
        // moverDefaults.podLabels/podAnnotations, applied to EVERY mover pod
        // (podLabels also to the Job; podAnnotations pod-only).
        pod_labels: resolved_mover.pod_labels.clone(),
        pod_annotations: resolved_mover.pod_annotations.clone(),
        labels,
        source_volume: None,
        repo_volume,
        creds_secrets,
        result_configmap: None,
        service_account: mover_identity.service_account.as_deref(),
        passthrough_env: ctx.mover_env_passthrough.clone(),
        extra_env: Vec::new(),
        annotations,
        // Inherit moverDefaults.cache (overlaid by the recipe's mover.cache) for the
        // kopia cache volume — but always per-run ephemeral, never the backup's warm
        // persistent PVC (see `verify_cache_volume`). Same `effective_cache` source as
        // the cache budgets in `build_verify_work_spec`.
        cache_volume: crate::cache::verify_cache_volume(
            crate::cache::effective_cache(
                repo,
                config.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
            )
            .as_ref(),
        ),
        // Deep verify restores into DEEP_SCRATCH_PATH; mount a writable volume there
        // (sized PVC if `capacity` is set, else emptyDir). Quick verify needs none.
        scratch_volume: match tier {
            VerifyTierKind::Deep => verification
                .deep
                .as_ref()
                .map(|deep| scratch_volume(crate::cache::effective_scratch(repo, deep).as_ref())),
            VerifyTierKind::Quick => None,
        },
        readiness_exec: None,
    };
    let job = jobs::build_job(&inputs)?;
    io::apply_mover_objects(&ctx.client, namespace, job_name, None, &job).await?;
    Ok(())
}

/// Build the verify mover work spec for a tier. Pure (no IO) so the tier→work-spec
/// mapping is unit-testable; the identity is the recipe's resolved source identity
/// so the deep tier restores the right snapshot.
///
/// `repository_key` is `Some(normalized repo key)` for a multi-repo policy's
/// per-repo run (metadata + the old-wire stamp fallback). `stamp_key` is the
/// `status.verificationStamps` key this run owns ([`stamp_key`]): the mover
/// then stamps that entry (a map-key merge, so concurrent cells never clobber)
/// instead of the flat `lastVerified`. `None` keeps the single-repo
/// single-member wire byte-identical.
///
/// `source_path` is the member's DERIVED kopia source path (#456), used
/// VERBATIM as the identity's path. Fallible: a policy whose CEL identity
/// cannot be resolved now PARKS with `Error::Validation` instead of silently
/// verifying a pathless sentinel that matches nothing and exits 0.
#[allow(clippy::too_many_arguments)]
pub fn build_verify_work_spec(
    config: &SnapshotPolicy,
    repo: &ResolvedRepository,
    namespace: &str,
    policy_name: &str,
    verification: &Verification,
    tier: VerifyTierKind,
    repository_key: Option<String>,
    source_path: Option<&str>,
    stamp_key: Option<String>,
) -> Result<MoverWorkSpec> {
    let tier = match tier {
        // M3 (issue #216 category sweep): quick.parallel/fileParallelism/
        // fileQueueLength/maxErrors were dormant plumbing — the workspec and kopia
        // client already supported them, but this arm hardcoded `None`, silently
        // dropping any value the user set on `verification.quick`.
        VerifyTierKind::Quick => VerifyTier::Quick(QuickVerify {
            verify_files_percent: verification.verify_files_percent,
            max_errors: verification.quick.as_ref().and_then(|q| q.max_errors),
            parallel: verification.quick.as_ref().and_then(|q| q.parallel),
            file_parallelism: verification.quick.as_ref().and_then(|q| q.file_parallelism),
            file_queue_length: verification
                .quick
                .as_ref()
                .and_then(|q| q.file_queue_length),
        }),
        VerifyTierKind::Deep => VerifyTier::Deep(DeepVerify {
            scratch_path: DEEP_SCRATCH_PATH.to_string(),
            // The mover resolves the latest snapshot for the identity itself.
            snapshot_id: None,
            parallel: verification.deep.as_ref().and_then(|d| d.parallel),
        }),
    };
    // The member's identity, so BOTH tiers scope to the right kopia source: the
    // deep tier restores that member's newest snapshot, and the quick tier's
    // `--sources` filter actually matches manifests instead of falling through
    // to a relative path and exiting 0 on zero of them (#456).
    let identity = verify_identity_for(config, namespace, repo, source_path)?;
    Ok(MoverWorkSpec {
        version: 1,
        operation: Operation::Verify(VerifyOp {
            tier,
            success_expr: verification.success_expr.clone(),
            repository_key,
            stamp_key,
        }),
        identity,
        repository: backend_to_repository_connect(&repo.backend, repo.ca_bundle_pem.clone()),
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            kind: "SnapshotPolicy".to_string(),
            name: policy_name.to_string(),
            namespace: namespace.to_string(),
            claim_key: None,
        },
        hook_plan: Default::default(),
        options: MoverOptions::default(),
        cache: crate::cache::cache_tuning(
            crate::cache::effective_cache(
                repo,
                config.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
            )
            .as_ref(),
        ),
        throttle: io::throttle_spec(repo.mover_defaults.as_ref()),
    })
}

/// Resolve the deep-verify scratch volume from the **effective** scratch config
/// (`moverDefaults.scratch` overlaid by `verification.deep`, via
/// [`crate::cache::effective_scratch`]). Mirrors [`crate::cache::resolve_cache_volume`]'s
/// capacity gate: a sized `capacity` provisions a fresh generic ephemeral volume (a
/// PVC bound to the pod's lifetime, auto-GC'd, honoring `storageClassName`); an unset
/// `capacity` falls back to an `emptyDir` (node-ephemeral, zero-config, writable by
/// the non-root mover via the pod's `fsGroup`). Never `Pvc` — scratch is discarded
/// after each run, so a persistent PVC would be wrong. Pure (no IO): unlike the
/// cache there is no owned PVC to provision, so this is unit-testable directly.
fn scratch_volume(effective: Option<&ScratchDefaults>) -> CacheVolume {
    match effective.and_then(|s| s.capacity.clone()) {
        Some(capacity) => CacheVolume::Ephemeral {
            capacity,
            storage_class: effective.and_then(|s| s.storage_class_name.clone()),
        },
        None => CacheVolume::EmptyDir,
    }
}

/// Whether a policy's deep-verify scratch `storageClassName` is a silent no-op
/// (set, but with no effective `capacity`, so scratch is an `emptyDir` — which has
/// no StorageClass), with the actionable message to surface. Computed from the
/// **merged** `moverDefaults.scratch` + `verification.deep`
/// (via [`crate::cache::effective_scratch`]). `None` when the policy has no
/// `verification.deep` (nothing to report); `Some { ignored: false }` is the normal
/// consistent state (so the condition self-clears `True`→`False` once a capacity is
/// added — never an orphaned `True`).
pub struct ScratchStorageClassState {
    /// `true` ⇒ the storageClass is being ignored (no-op); `false` ⇒ honored/consistent.
    pub ignored: bool,
    /// Human-/machine-actionable message for the condition + Warning Event.
    pub message: String,
}

/// See [`ScratchStorageClassState`].
pub fn scratch_storage_class_state(
    repo: &ResolvedRepository,
    verification: &Verification,
) -> Option<ScratchStorageClassState> {
    let deep = verification.deep.as_ref()?;
    let effective = crate::cache::effective_scratch(repo, deep);
    let storage_class = effective
        .as_ref()
        .and_then(|s| s.storage_class_name.clone());
    let has_capacity = effective
        .as_ref()
        .and_then(|s| s.capacity.as_ref())
        .is_some();
    Some(match (storage_class, has_capacity) {
        (Some(sc), false) => ScratchStorageClassState {
            ignored: true,
            message: format!(
                "deep-verify scratch storageClassName '{sc}' has no effect without a capacity \
                 (an emptyDir has no StorageClass); set verification.deep.capacity or \
                 moverDefaults.scratch.capacity to provision a sized PVC"
            ),
        },
        _ => ScratchStorageClassState {
            ignored: false,
            message: "deep-verify scratch volume configuration is consistent".to_string(),
        },
    })
}

/// The controller-side fold of per-repo verification state (#368): the
/// authoritative `status.verification` Vec + flat `lastVerified`, derived from
/// the CURRENT repository set, the previous Vec, and the movers' entry-keyed
/// `verificationStamps`.
#[derive(Debug, Clone, PartialEq)]
pub struct FoldedVerification {
    /// One entry per CURRENT repository, in spec order — entries for
    /// repositories no longer in the spec are PRUNED (audit m10: a stale entry
    /// would pin the flat MIN forever).
    pub entries: Vec<kopiur_api::snapshot_policy::RepoVerification>,
    /// The flat `status.lastVerified` for the multi-repo shape: the MINIMUM
    /// across the current repositories' timestamps — "everything is verified
    /// as of T" — and `None` until EVERY current repository has verified at
    /// least once (a partially-verified fleet must not display a reassuring
    /// timestamp).
    pub flat: Option<String>,
}

/// **Pure.** Fold the movers' entry-keyed stamps into the per-repo Vec the
/// controller owns (single writer: movers only ever merge-patch their own
/// `verificationStamps[<key>]` — a map key merge that cannot clobber a sibling
/// — and this fold is the only writer of `verification` itself).
///
/// Design note (the race the plan demands a decision on): SSA with per-entry
/// managers was rejected because `verification` is keyed by the `repository`
/// SUB-OBJECT, which cannot be an `x-kubernetes-list-map-keys` key (map keys
/// must be scalars), so per-entry SSA merging would have forced a redundant
/// scalar key field into the user-facing API. The stamp map gets the same
/// clobber-freedom from plain RFC 7396 map-key merging with no API distortion;
/// the race test lives in `kopiur-mover`'s
/// `concurrent_per_repo_stamps_both_survive_merge_patching` (write side) and
/// [`tests::fold_folds_both_concurrent_stamps`] (read side).
///
/// Per-repo timestamps are monotonic: an entry's `lastVerified` only advances
/// (`max` of the prior entry and the stamp), so re-reading a stale stamp is
/// idempotent and stamps never need deleting on consumption — only a repo's
/// REMOVAL from the spec prunes its entry (and its stamp key, by the caller
/// writing the pruned map back).
pub fn fold_verification(
    current: &[(kopiur_api::common::RepositoryRef, String)],
    existing: &[kopiur_api::snapshot_policy::RepoVerification],
    stamps: &BTreeMap<String, String>,
    members: &[String],
    ns: &str,
) -> FoldedVerification {
    use kopiur_api::snapshot_policy::RepoVerification;
    let entries: Vec<RepoVerification> = current
        .iter()
        .map(|(rref, key)| RepoVerification {
            repository: rref.clone(),
            last_verified: repo_cell_min(existing, stamps, members, key, ns),
        })
        .collect();
    let flat = entries
        .iter()
        .map(|e| e.last_verified.as_deref())
        .collect::<Option<Vec<_>>>()
        .and_then(|all| all.into_iter().min_by_key(|ts| rfc3339_or_min(ts)))
        .map(str::to_string);
    FoldedVerification { entries, flat }
}

/// **Pure.** One repository's folded `lastVerified`: the MIN over its member
/// cells (#456), or its single repo-keyed stamp when the policy does not fan
/// out.
///
/// The no-member arm keeps the #368 monotonic `max(prior entry, stamp)` so a
/// re-read stale stamp is idempotent. The member arm deliberately does NOT
/// carry the prior entry forward: a repository whose member set GREW is
/// genuinely no longer fully verified, and pinning the old (higher) value would
/// make a partially-verified repository look complete — the exact class of lie
/// #456 was.
fn repo_cell_min(
    existing: &[kopiur_api::snapshot_policy::RepoVerification],
    stamps: &BTreeMap<String, String>,
    members: &[String],
    key: &str,
    ns: &str,
) -> Option<String> {
    if members.is_empty() {
        let prior = existing
            .iter()
            .find(|e| kopiur_api::common::repo_key(&e.repository, ns) == key)
            .and_then(|e| e.last_verified.as_deref());
        return later_rfc3339(prior, stamps.get(key).map(String::as_str));
    }
    members
        .iter()
        .map(|m| {
            stamp_key(Some(key), Some(m))
                .and_then(|k| stamps.get(&k))
                .map(String::as_str)
        })
        .collect::<Option<Vec<_>>>()
        .and_then(|all| all.into_iter().min_by_key(|ts| rfc3339_or_min(ts)))
        .map(str::to_string)
}

/// **Pure.** The later of two RFC3339 timestamps (an unparseable side loses to
/// a parseable one; both unparseable/absent → `None`).
fn later_rfc3339(a: Option<&str>, b: Option<&str>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => {
            if rfc3339_or_min(a) >= rfc3339_or_min(b) {
                Some(a.to_string())
            } else {
                Some(b.to_string())
            }
        }
        (Some(one), None) | (None, Some(one)) => Some(one.to_string()),
        (None, None) => None,
    }
}

/// RFC3339 → `DateTime<Utc>`, with an unparseable value sorting first (so it
/// never wins a `max` and never anchors a `min` reassuringly late).
fn rfc3339_or_min(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// Resolve the identity for ONE verify cell, reusing the SAME kernel entry point
/// the #443 restore fan-out uses ([`crate::snapshot_policy::config_identity_for_path`]).
///
/// `path: Some(_)` is the member's derived kopia source path, fed in as a
/// `sourcePathOverride` so the identity kernel uses it VERBATIM instead of
/// re-deriving `/pvc/<name>` from the FIRST source's PVC name — which for a
/// `pvcSelector` policy derived nothing at all, leaving the pathless
/// `user@host:` filter behind #456.
///
/// `path: None` is the pre-#456 shape, byte-for-byte: the governing source's
/// own `pvc`/`nfs`/`sourcePathOverride`, and a PATHLESS identity for a
/// zero-source legacy policy (`config_identity_for_path`'s `/data` fallback
/// only applies when the policy HAS sources, which is the same fallback the
/// backup mover takes).
///
/// **Two documented behaviour changes** relative to the old `verify_identity`:
///
/// 1. A policy that has sources but derives no path resolves `/data` rather
///    than `""` — the path the BACKUP actually recorded it under.
/// 2. A failed identity resolution (a CEL `usernameExpr`/`hostnameExpr` that
///    cannot render) now PROPAGATES as `Error::Validation` and parks the
///    policy. It used to be masked as a `kopiur-verify@<ns>:` sentinel with an
///    empty path, i.e. a run that verified nothing and reported success.
fn verify_identity_for(
    config: &SnapshotPolicy,
    namespace: &str,
    repo: &ResolvedRepository,
    path: Option<&str>,
) -> Result<ResolvedIdentity> {
    let r = crate::snapshot_policy::config_identity_for_path(
        config,
        namespace,
        repo.identity_defaults.as_ref(),
        path,
    )?;
    Ok(ResolvedIdentity {
        username: r.username,
        hostname: r.hostname,
        source_path: r.source_path.unwrap_or_default(),
    })
}

/// Whether the policy's resolved repository already carries **discovered**
/// (adopted) or **replicated** snapshots — the #168 verification escape hatch.
/// A cheap labeled LIST scoped by the repo-uid label plus a SET-BASED origin
/// selector (`origin in (discovered, replicated)`): discovered rows are what
/// the catalog scanner stamps on every materialized foreign snapshot
/// (`crate::catalog`), and replicated rows are the dest-side copy CRs a
/// `SnapshotReplication` mints — a destination repository holding ONLY copies
/// has verifiable content and must not look empty to this gate. `true` as soon
/// as one of either exists. The [`DiscoveredProbeScope`] (from
/// [`discovered_probe_scope`]) selects namespaced vs cluster-wide so an
/// adopted `ClusterRepository`'s scattered rows are seen. Thin IO over the
/// pure [`verification_unlocked`] gate.
async fn discovered_identities(
    client: &kube::Client,
    scope: &DiscoveredProbeScope,
    repo_uid: &str,
    wanted: &BTreeSet<IdentityKey>,
) -> Result<BTreeSet<IdentityKey>> {
    let api: Api<Snapshot> = match scope {
        DiscoveredProbeScope::Namespace(ns) => Api::namespaced(client.clone(), ns),
        DiscoveredProbeScope::ClusterWide => Api::all(client.clone()),
    };
    let selector = format!(
        "{REPOSITORY_UID_LABEL}={repo_uid},{ORIGIN_LABEL} in ({},{})",
        Origin::Discovered.label_value(),
        Origin::Replicated.label_value()
    );
    let mut found: BTreeSet<IdentityKey> = BTreeSet::new();
    let mut token: Option<String> = None;
    loop {
        let mut lp = ListParams::default()
            .labels(&selector)
            .limit(DISCOVERED_PROBE_PAGE_SIZE);
        if let Some(t) = &token {
            lp = lp.continue_token(t);
        }
        let page = api.list(&lp).await?;
        token = page.metadata.continue_.clone().filter(|t| !t.is_empty());
        // Only the WANTED identity triples are kept, never the objects, so a
        // shared repository with a large foreign catalog costs memory bounded
        // by the gated member count rather than by the catalog. Paginated
        // rather than `.limit()`ed because COMPLETENESS is load-bearing: a
        // truncated read is indistinguishable from "this identity has no
        // adopted snapshot" and would leave an adopted repository's
        // verification silently gated forever.
        found.extend(
            page.items
                .iter()
                .filter_map(|s| s.status.as_ref()?.snapshot.as_ref())
                .map(|info| identity_key(&info.identity))
                .filter(|k| wanted.contains(k)),
        );
        // Every gated member is accounted for: no later page can change an
        // answer, so stop. This does NOT weaken completeness — it only skips
        // reading rows whose identities we already have.
        if found.len() == wanted.len() || token.is_none() {
            return Ok(found);
        }
    }
}

/// **Pure.** The verification tier an existing verify Job runs, read off its
/// per-slot name.
///
/// Every shape [`verify_job_name`] has ever produced ends in
/// `-vfy-<q|d>[-<r6>][-m<m6>]-<unix>`, and nothing after the marker contains
/// another `-vfy-`, so the LAST occurrence is authoritative even for a policy
/// whose own name contains the marker. Derived from the name rather than a new
/// label precisely so it works on Jobs already in flight across an upgrade.
///
/// `None` = "cannot tell", which callers treat as blocking. There is no tier
/// label to add retroactively, so an unreadable name must never be assumed
/// harmless.
fn job_tier(job: &Job) -> Option<VerifyTierKind> {
    let (_, tail) = job.metadata.name.as_deref()?.rsplit_once("-vfy-")?;
    if tail.starts_with("q-") {
        Some(VerifyTierKind::Quick)
    } else if tail.starts_with("d-") {
        Some(VerifyTierKind::Deep)
    } else {
        None
    }
}

/// **Pure.** Whether one existing verify Job holds the single-flight slot of
/// the cell described by (`tier`, `member6`).
///
/// This is the client-side half of the #456 upgrade guard. The LIST selector
/// deliberately omits [`VERIFY_MEMBER_LABEL`] (see [`verify_flight_selector`]),
/// so the filtering happens here:
///
/// * a Job with **no** member label, or whose name yields no tier, blocks
///   EVERY cell. That is a Job minted by an operator that predates this
///   design — for the deep tier, letting N fresh members start beside it would
///   mean N+1 concurrent scratch restores;
/// * **quick** is per-member: only the same member's Job holds the slot, so
///   sibling quick members run concurrently. While not fanning out
///   (`member6: None`) any labelled Job blocks, which is conservative and
///   transient;
/// * **deep** is sequential across members, but only against other DEEP Jobs.
///   Blocking it on any in-flight QUICK Job (as the first cut did) starves the
///   drill on a wide policy: with N members and a tight quick cron the chance
///   that some quick Job is running at deep-due time scales with N, and a
///   held deep also suppresses that member's own quick (`due_tier` prefers
///   deep). The sequencing property that matters — at most one deep scratch
///   restore per (policy, repository), across reconciles and operator
///   restarts, not merely within one pass — is preserved.
fn job_blocks_cell(job: &Job, tier: VerifyTierKind, member6: Option<&str>) -> bool {
    match (job.labels().get(VERIFY_MEMBER_LABEL), job_tier(job)) {
        (None, _) | (_, None) => true,
        (Some(theirs), Some(job_tier)) => match tier {
            VerifyTierKind::Quick => member6.is_none_or(|mine| mine == theirs),
            VerifyTierKind::Deep => job_tier == VerifyTierKind::Deep,
        },
    }
}

/// **Pure.** The single-flight LIST selector for one (policy, repository)
/// scope.
///
/// It **must never** contain [`VERIFY_MEMBER_LABEL`]: a Job minted by an
/// operator predating that label would stop matching, and the reconciler would
/// spawn one fresh Job per member alongside it — for the deep tier, N+1
/// concurrent scratch restores. The member (and tier) narrowing happens
/// client-side in [`job_blocks_cell`]. Extracted as a pure function so that
/// invariant is actually asserted
/// ([`tests::the_flight_selector_never_narrows_by_member`]) rather than left to
/// a reviewer noticing a one-line change.
fn verify_flight_selector(policy_name: &str, repo6: Option<&str>) -> String {
    let mut selector =
        format!("{COMPONENT_LABEL}={VERIFY_COMPONENT},{VERIFY_INSTANCE_LABEL}={policy_name}");
    if let Some(r6) = repo6 {
        selector.push_str(&format!(",{VERIFY_REPO_LABEL}={r6}"));
    }
    selector
}

/// Thin IO: the #168 escape-hatch index for ONE repository — hoisted out of
/// [`verify_step`] by the caller so a fanned-out policy runs it once per
/// repository instead of once per (repository x member) cell.
///
/// `wanted` is [`probe_identities`]' output: which of this repository's
/// identities are still gated and therefore worth asking about. An empty set
/// skips the read entirely, with no request at all.
///
/// Probed in the repository's OWN namespace (not the policy's — `RepositoryRef`
/// allows a cross-namespace reference), or cluster-wide for a cluster-scoped
/// repository (`repo_namespace == None`), which scatters its discovered rows
/// across per-identity namespaces.
pub async fn discovered_identity_index(
    ctx: &Context,
    repo: &ResolvedRepository,
    wanted: &BTreeSet<IdentityKey>,
) -> Result<BTreeSet<IdentityKey>> {
    if wanted.is_empty() {
        return Ok(BTreeSet::new());
    }
    let scope = discovered_probe_scope(repo.repo_namespace.as_deref());
    discovered_identities(&ctx.client, &scope, &repo.owner_ref.uid, wanted).await
}

/// Whether any non-terminal verify Job holds this cell's single-flight slot.
///
/// `repo6` (a multi-repo policy's per-repo run) narrows the LIST by
/// [`VERIFY_REPO_LABEL`] so the gate is per (policy, repository) — repo A's
/// in-flight verify must not block repo B's. The single-repo shape (`None`)
/// keeps the policy-wide selector, which also matches in-flight Jobs from
/// older operators that predate the repo label.
///
/// `tier` and `member6` narrow further, but **client-side only**
/// ([`job_blocks_cell`]): adding either to the LIST would make a legacy
/// unlabelled Job invisible and spawn N new Jobs alongside it.
async fn has_active_verify_job(
    job_api: &Api<Job>,
    policy_name: &str,
    repo6: Option<&str>,
    tier: VerifyTierKind,
    member6: Option<&str>,
) -> Result<bool> {
    let jobs = job_api
        .list(&ListParams::default().labels(&verify_flight_selector(policy_name, repo6)))
        .await?;
    Ok(jobs
        .items
        .iter()
        .any(|j| job_terminal_state(j).is_none() && job_blocks_cell(j, tier, member6)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot_schedule::{jitter_defaults, tz_defaults};
    use kopiur_api::common::CronSpec;
    use kopiur_api::snapshot_policy::{DeepVerification, QuickVerification};

    fn verification(quick: Option<&str>, deep: Option<&str>) -> Verification {
        Verification {
            quick: quick.map(|c| QuickVerification {
                schedule: Some(CronSpec {
                    cron: c.into(),
                    jitter: None,
                    timezone: None,
                }),
                parallel: None,
                file_parallelism: None,
                file_queue_length: None,
                max_errors: None,
            }),
            deep: deep.map(|c| DeepVerification {
                schedule: CronSpec {
                    cron: c.into(),
                    jitter: None,
                    timezone: None,
                },
                storage_class_name: None,
                capacity: None,
                parallel: None,
            }),
            success_expr: None,
            verify_files_percent: None,
        }
    }

    #[test]
    fn first_ever_reconcile_is_due_and_prefers_deep() {
        // No lastVerified, but UNLOCKED (a successful backup exists) → both due; deep
        // wins (it subsumes quick). The catch-up semantics are preserved once unlocked.
        let v = verification(Some("*/5 * * * *"), Some("0 3 * * 0"));
        let (tier, _slot) = due_tier(&v, "seed", None, Utc::now(), None, true).expect("due");
        assert_eq!(tier, VerifyTierKind::Deep);
    }

    #[test]
    fn quick_only_is_due_when_no_deep() {
        let v = verification(Some("*/5 * * * *"), None);
        let (tier, _) = due_tier(&v, "seed", None, Utc::now(), None, true).expect("due");
        assert_eq!(tier, VerifyTierKind::Quick);
    }

    // A fixed mid-slot instant (Saturday 12:02:33 UTC) for tests that anchor a
    // last-run time relative to "now". With a live Utc::now(), a run landing in
    // the first second after a cron boundary (e.g. 05:15:00 for `*/5 * * * *`)
    // puts a genuinely new slot between the `now - 1s` anchor and now, and the
    // kernel rightly fires — a ~1/300 CI flake, not an operator bug. Same
    // pinning as `maintenance::tests::pinned_now`.
    fn pinned_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-06T12:02:33Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn not_due_right_after_a_run() {
        let now = pinned_now();
        let just = now - chrono::Duration::seconds(1);
        let v = verification(Some("*/5 * * * *"), Some("0 3 * * 0"));
        assert!(
            due_tier(&v, "seed", Some(just), now, None, true).is_none(),
            "a tier that just ran must not be immediately due again"
        );
    }

    #[test]
    fn no_schedules_is_never_due() {
        let v = verification(None, None);
        assert!(due_tier(&v, "seed", None, Utc::now(), None, true).is_none());
        assert!(due_tier(&v, "seed", None, Utc::now(), None, true).is_none());
    }

    #[test]
    fn old_shape_quick_without_schedule_is_disabled_not_paniced() {
        // GitHub #174 decode-tolerance: a stale persisted `quick: {cron: ...}` object
        // decodes as `quick: Some(QuickVerification { schedule: None })`. The reconciler
        // must treat it as "quick tier disabled" — never due, no panic/wedge. (New
        // writes of this shape are blocked at admission.)
        let v = Verification {
            quick: Some(QuickVerification {
                schedule: None,
                ..Default::default()
            }),
            deep: None,
            success_expr: None,
            verify_files_percent: None,
        };
        assert!(
            due_tier(&v, "seed", None, Utc::now(), None, true).is_none(),
            "quick with no schedule must be disabled, not due"
        );
        // And it doesn't spin either: with no live schedule at all, the wakeup floors
        // at the steady cadence rather than the 30s past-due hot loop, so an unmigrated
        // quick-only policy doesn't re-reconcile every 30s forever.
        let wake = next_verify_wakeup(&v, "seed", None, Utc::now(), None);
        assert_eq!(wake, REQUEUE_GATED);
        // idle_requeue agrees when the policy is unlocked (a successful backup exists):
        // no live schedule => steady cadence, not the hot loop.
        let idle = idle_requeue(&v, "seed", None, Utc::now(), None, true);
        assert_eq!(idle, REQUEUE_GATED);
    }

    // --- #168 gate: no verify before a verifiable snapshot exists ---

    #[test]
    fn gated_when_no_successful_and_no_discovered_is_not_due() {
        // The #168 regression: a brand-new policy (no successful backup, adopted-repo
        // escape hatch closed) must NOT be due, even though tier_after makes the
        // first slot past-due. Fails on pre-gate code (which returned Some(Deep)).
        let v = verification(Some("*/5 * * * *"), Some("0 3 * * 0"));
        let unlocked = verification_unlocked(false, false);
        assert!(!unlocked);
        assert!(
            due_tier(&v, "seed", None, Utc::now(), None, unlocked).is_none(),
            "no snapshot to verify → nothing due"
        );
    }

    #[test]
    fn unlocked_by_successful_snapshot_is_due() {
        let v = verification(Some("*/5 * * * *"), Some("0 3 * * 0"));
        let unlocked = verification_unlocked(true, false);
        assert!(unlocked);
        assert!(due_tier(&v, "seed", None, Utc::now(), None, unlocked).is_some());
    }

    #[test]
    fn unlocked_by_discovered_snapshot_is_due_adopted_repo_escape_hatch() {
        // Adopted repo (05-adopt-existing-repo): no Succeeded snapshot for this policy,
        // but the repo carries discovered CRs. Deep verify works there, so the gate
        // must open on the discovered signal alone.
        let v = verification(Some("*/5 * * * *"), Some("0 3 * * 0"));
        let unlocked = verification_unlocked(false, true);
        assert!(unlocked);
        assert!(due_tier(&v, "seed", None, Utc::now(), None, unlocked).is_some());
    }

    #[test]
    fn namespaced_repo_same_namespace_as_policy_probes_that_namespace() {
        // Same-namespace ref (the common case): the repo's own namespace happens to
        // equal the referencing policy's — unchanged behavior from before this fix.
        assert_eq!(
            discovered_probe_scope(Some("billing")),
            DiscoveredProbeScope::Namespace("billing".into())
        );
    }

    #[test]
    fn namespaced_repo_cross_namespace_ref_probes_the_repos_own_namespace() {
        // Cross-namespace RepositoryRef (crates/api/src/validate/repository.rs:11-13):
        // a SnapshotPolicy can live in one namespace while referencing an adopted
        // Repository that lives in another. Discovered Snapshot CRs materialize in
        // the REPOSITORY's own namespace (Placement::Namespace, repository.rs:1299),
        // so the probe must follow the repo, not the policy. A pre-fix
        // `discovered_probe_scope(false, policy_namespace)` would probe "billing"
        // here and never see the discovered rows in "storage" — this assertion fails
        // on that code.
        assert_eq!(
            discovered_probe_scope(Some("storage")),
            DiscoveredProbeScope::Namespace("storage".into())
        );
    }

    #[test]
    fn cluster_repo_probes_cluster_wide_so_adopted_repos_unlock() {
        // Regression for the escape hatch no-op: a ClusterRepository's discovered rows
        // land in per-identity namespaces (Placement::Cluster) — generally NOT any
        // single namespace — so the probe MUST be cluster-wide, or an adopted
        // ClusterRepository never unlocks verification before its first child backup.
        assert_eq!(
            discovered_probe_scope(None),
            DiscoveredProbeScope::ClusterWide
        );
    }

    #[test]
    fn gated_requeue_is_steady_not_the_30s_hot_loop() {
        // While gated, the past-due catch-up slot must NOT drive next_verify_wakeup's
        // 30s REQUEUE_RUNNING; idle_requeue returns the steady cadence instead.
        let now = Utc::now();
        let v = verification(Some("*/5 * * * *"), Some("0 3 * * 0"));
        let gated = idle_requeue(&v, "seed", None, now, None, false);
        assert_eq!(gated, REQUEUE_GATED);
        assert!(
            gated > REQUEUE_RUNNING,
            "gated requeue must not be the 30s hot loop"
        );
        // Unlocked with a past-due slot DOES want the prompt 30s cadence (the caller
        // then spawns/tracks the Job) — guards that the gate only affects the locked path.
        assert_eq!(
            idle_requeue(&v, "seed", None, now, None, true),
            REQUEUE_RUNNING,
            "an unlocked past-due policy keeps the prompt cadence"
        );
    }

    #[test]
    fn wakeup_is_capped() {
        let now = Utc::now();
        let just = now - chrono::Duration::seconds(1);
        // Daily deep, ran moments ago → next ~24h out, but capped to the heartbeat.
        let v = verification(None, Some("0 3 * * *"));
        assert!(next_verify_wakeup(&v, "seed", Some(just), now, None) <= REQUEUE_CAP);
    }

    #[test]
    fn due_tier_honors_repo_schedule_default_timezone() {
        // A cron pinned to a specific wall-clock hour is due/not-due depending on
        // which timezone it's evaluated in — proof the repo default actually
        // reaches the scheduling decision, not just the CronSpec's own field.
        let now = DateTime::parse_from_rfc3339("2026-06-09T05:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let last_verified = Some(now - chrono::Duration::hours(2));
        // 05:00 UTC has already passed; 05:00 America/Los_Angeles (UTC-7 in June)
        // is still ~6.5h out. No own timezone on the CronSpec — must come from
        // the repo default.
        let v = verification(Some("0 5 * * *"), None);
        assert!(
            due_tier(&v, "seed", last_verified, now, None, true).is_some(),
            "UTC (no repo default) → 05:00 UTC has already passed"
        );
        assert!(
            due_tier(
                &v,
                "seed",
                last_verified,
                now,
                Some(&tz_defaults("America/Los_Angeles")),
                true
            )
            .is_none(),
            "repo scheduleDefaults.timezone must shift the evaluated slot"
        );
    }

    #[test]
    fn due_tier_own_timezone_wins_over_repo_default() {
        let now = DateTime::parse_from_rfc3339("2026-06-09T05:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let last_verified = Some(now - chrono::Duration::hours(2));
        let mut v = verification(Some("0 5 * * *"), None);
        v.quick
            .as_mut()
            .unwrap()
            .schedule
            .as_mut()
            .unwrap()
            .timezone = Some("UTC".into());
        // Own timezone (UTC) says the slot is due; the repo default
        // (America/Los_Angeles, which would push the slot hours into the future)
        // must be ignored.
        assert!(
            due_tier(
                &v,
                "seed",
                last_verified,
                now,
                Some(&tz_defaults("America/Los_Angeles")),
                true
            )
            .is_some(),
            "the CronSpec's own timezone must win over the repo default"
        );
    }

    // --- scheduleDefaults.jitter cascade --------------------------------------
    // per-cron `jitter` -> repo `scheduleDefaults.jitter` -> none, for BOTH the
    // quick and deep tiers (they share `slot_for`). Asserted by comparing slots:
    // the offset is `fnv1a(seed, slot)`-derived, so "the window was applied" is
    // proven by matching an explicit own value and differing from the un-jittered
    // slot — seed-independent, so it cannot rot.

    /// The `after` anchor the jitter tests below share.
    fn jitter_after() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-09T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn cron_spec(cron: &str, jitter: Option<&str>) -> CronSpec {
        CronSpec {
            cron: cron.into(),
            jitter: jitter.map(str::to_string),
            timezone: None,
        }
    }

    #[test]
    fn slot_for_inherits_the_repo_default_jitter_for_both_tiers() {
        let after = jitter_after();
        // One `CronSpec` shape drives both tiers, so a single pair of assertions
        // covers quick and deep; the seed is the only thing that differs and it is
        // held constant here.
        for seed in ["policy-uid-quick", "policy-uid-deep"] {
            let bare = slot_for(seed, &cron_spec("0 5 * * *", None), after, None).unwrap();
            let inherited = slot_for(
                seed,
                &cron_spec("0 5 * * *", None),
                after,
                Some(&jitter_defaults("1h")),
            )
            .unwrap();
            assert_ne!(
                inherited, bare,
                "an inherited window must actually spread the slot ({seed})"
            );
            assert_eq!(
                inherited,
                slot_for(seed, &cron_spec("0 5 * * *", Some("1h")), after, None).unwrap(),
                "inheritance must resolve to the same window as setting it directly ({seed})"
            );
        }
    }

    #[test]
    fn slot_for_own_jitter_wins_over_the_repo_default() {
        let after = jitter_after();
        let spec = cron_spec("0 5 * * *", Some("1h"));
        assert_eq!(
            slot_for("seed", &spec, after, Some(&jitter_defaults("10m"))).unwrap(),
            slot_for("seed", &spec, after, None).unwrap(),
            "an own per-cron jitter must ignore the repo default entirely"
        );
    }

    #[test]
    fn slot_for_with_neither_jitter_is_the_bare_cron_slot() {
        let after = jitter_after();
        let spec = cron_spec("0 5 * * *", None);
        let slot = slot_for("seed", &spec, after, None).unwrap();
        assert_eq!(slot.to_rfc3339(), "2026-06-09T05:00:00+00:00");
        // Byte-identical regression: a timezone-only repo default is the
        // pre-jitter world and must not move the slot.
        assert_eq!(
            slot_for("seed", &spec, after, Some(&tz_defaults("UTC"))).unwrap(),
            slot
        );
    }

    #[test]
    fn due_tier_honors_an_inherited_jitter_window() {
        // The wiring, through the public entry point: at 05:15 the un-jittered
        // 05:00 slot is due, but a 1h inherited window pushes this seed's slot past
        // `now` — so the tier is NOT yet due. (The offset for this seed/slot is
        // asserted to be >15m by the first assertion pair, which is what makes the
        // second one meaningful.)
        let now = DateTime::parse_from_rfc3339("2026-06-09T05:15:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let last_verified = Some(now - chrono::Duration::hours(6));
        let v = verification(Some("0 5 * * *"), None);
        assert!(
            due_tier(&v, "seed", last_verified, now, None, true).is_some(),
            "un-jittered: 05:00 has passed"
        );
        let jittered = slot_for(
            "seed",
            &cron_spec("0 5 * * *", None),
            now - chrono::Duration::hours(6),
            Some(&jitter_defaults("1h")),
        )
        .unwrap();
        assert!(
            jittered > now,
            "fixture precondition: this seed's 1h offset must exceed 15m (got {jittered})"
        );
        assert!(
            due_tier(
                &v,
                "seed",
                last_verified,
                now,
                Some(&jitter_defaults("1h")),
                true
            )
            .is_none(),
            "an inherited jitter window must move the tier's due decision"
        );
    }

    #[test]
    fn verify_job_name_is_deterministic_and_bounded() {
        let slot = DateTime::parse_from_rfc3339("2026-06-09T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let n = verify_job_name("postgres-data", VerifyTierKind::Quick, slot, None, None);
        assert!(n.len() <= 52);
        assert!(n.starts_with("postgres-data-vfy-q-"));
        assert_eq!(
            n,
            verify_job_name("postgres-data", VerifyTierKind::Quick, slot, None, None)
        );
        // Quick vs deep differ.
        assert_ne!(
            n,
            verify_job_name("postgres-data", VerifyTierKind::Deep, slot, None, None)
        );
        // Long names truncate+hash within budget.
        let long = "a-very-long-snapshot-policy-name-that-blows-the-dns-label-budget";
        assert!(verify_job_name(long, VerifyTierKind::Deep, slot, None, None).len() <= 52);
    }

    #[test]
    fn verify_job_name_multi_repo_gains_r6_segment_single_repo_byte_identical() {
        // #368: the <r6> segment exists ONLY for the multi-repo shape. The
        // single-repo name must stay byte-identical to the pre-feature format
        // (slot continuity: an in-flight Job must still be found by name across
        // the upgrade), so the tag is spliced BETWEEN the tier and the unix
        // slot, never appended to the single-repo form.
        let slot = DateTime::parse_from_rfc3339("2026-06-09T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let unix = slot.timestamp();
        assert_eq!(
            verify_job_name("pg", VerifyTierKind::Quick, slot, None, None),
            format!("pg-vfy-q-{unix}"),
            "single-repo name is the exact legacy format"
        );
        let r6 = crate::naming::repo_tag6("Repository/backups/nas");
        let multi = verify_job_name("pg", VerifyTierKind::Quick, slot, Some(&r6), None);
        assert_eq!(multi, format!("pg-vfy-q-{r6}-{unix}"));
        assert!(multi.len() <= 52);
        // Distinct repos → distinct per-slot names (concurrent Jobs coexist).
        let other = crate::naming::repo_tag6("ClusterRepository/offsite");
        assert_ne!(
            multi,
            verify_job_name("pg", VerifyTierKind::Quick, slot, Some(&other), None)
        );
        // Long policy names stay within budget with the tag present.
        let long = "a-very-long-snapshot-policy-name-that-blows-the-dns-label-budget";
        assert!(verify_job_name(long, VerifyTierKind::Deep, slot, Some(&r6), None).len() <= 52);
    }

    #[test]
    fn verify_repo_label_is_group_prefixed() {
        // A typo'd prefix would silently break the per-repo single-flight
        // selector (same tripwire as kopiur_api's label test).
        assert!(
            VERIFY_REPO_LABEL.starts_with("kopiur.home-operations.com/"),
            "{VERIFY_REPO_LABEL} must be group-prefixed"
        );
        assert_ne!(
            VERIFY_REPO_LABEL, VERIFY_INSTANCE_LABEL,
            "repo scope must not collide with the policy scope"
        );
    }

    // --- #368 fold: entry-keyed per-repo verification state -------------------

    fn rref(kind: &str, ns: Option<&str>, name: &str) -> kopiur_api::common::RepositoryRef {
        serde_json::from_value(serde_json::json!({
            "kind": kind,
            "name": name,
            "namespace": ns,
        }))
        .expect("ref")
    }

    fn keyed(
        refs: &[kopiur_api::common::RepositoryRef],
    ) -> Vec<(kopiur_api::common::RepositoryRef, String)> {
        refs.iter()
            .map(|r| (r.clone(), kopiur_api::common::repo_key(r, "ns")))
            .collect()
    }

    #[test]
    fn fold_folds_both_concurrent_stamps() {
        // Read side of the clobber race test: two per-repo movers stamped
        // concurrently (map-key merge, proven non-clobbering on the write side
        // in kopiur-mover); the fold must surface BOTH.
        let repos = [
            rref("Repository", Some("ns"), "nas"),
            rref("ClusterRepository", None, "offsite"),
        ];
        let stamps: BTreeMap<String, String> = [
            (
                "Repository/ns/nas".to_string(),
                "2026-08-02T00:00:00+00:00".to_string(),
            ),
            (
                "ClusterRepository/offsite".to_string(),
                "2026-08-01T00:00:00+00:00".to_string(),
            ),
        ]
        .into();
        let folded = fold_verification(&keyed(&repos), &[], &stamps, &[], "ns");
        assert_eq!(folded.entries.len(), 2);
        assert_eq!(
            folded.entries[0].last_verified.as_deref(),
            Some("2026-08-02T00:00:00+00:00")
        );
        assert_eq!(
            folded.entries[1].last_verified.as_deref(),
            Some("2026-08-01T00:00:00+00:00")
        );
        // Flat = MIN across current repos ("everything verified as of T").
        assert_eq!(folded.flat.as_deref(), Some("2026-08-01T00:00:00+00:00"));
    }

    #[test]
    fn fold_is_monotonic_and_flat_absent_until_every_repo_verified() {
        let repos = [
            rref("Repository", Some("ns"), "nas"),
            rref("ClusterRepository", None, "offsite"),
        ];
        // Only repo A has ever verified → flat must stay absent (a
        // partially-verified fleet must not display a reassuring timestamp).
        let existing = vec![kopiur_api::snapshot_policy::RepoVerification {
            repository: repos[0].clone(),
            last_verified: Some("2026-08-02T00:00:00+00:00".into()),
        }];
        let folded = fold_verification(&keyed(&repos), &existing, &BTreeMap::new(), &[], "ns");
        assert_eq!(folded.entries.len(), 2);
        assert!(folded.entries[1].last_verified.is_none());
        assert!(folded.flat.is_none(), "flat requires EVERY current repo");

        // A stale stamp (older than the entry) never regresses the entry.
        let stale: BTreeMap<String, String> = [(
            "Repository/ns/nas".to_string(),
            "2026-07-01T00:00:00+00:00".to_string(),
        )]
        .into();
        let folded = fold_verification(&keyed(&repos), &existing, &stale, &[], "ns");
        assert_eq!(
            folded.entries[0].last_verified.as_deref(),
            Some("2026-08-02T00:00:00+00:00"),
            "per-repo timestamps are monotonic"
        );
    }

    #[test]
    fn fold_prunes_entries_for_repos_no_longer_in_spec() {
        // Audit m10: a stale entry for a REMOVED repo would pin the flat MIN
        // forever. The fold keys everything off the CURRENT repo set.
        let current = [rref("Repository", Some("ns"), "nas")];
        let existing = vec![
            kopiur_api::snapshot_policy::RepoVerification {
                repository: rref("Repository", Some("ns"), "nas"),
                last_verified: Some("2026-08-02T00:00:00+00:00".into()),
            },
            kopiur_api::snapshot_policy::RepoVerification {
                repository: rref("ClusterRepository", None, "removed"),
                last_verified: Some("2020-01-01T00:00:00+00:00".into()),
            },
        ];
        // The removed repo also left a stamp behind — it must not resurrect.
        let stamps: BTreeMap<String, String> = [(
            "ClusterRepository/removed".to_string(),
            "2026-08-03T00:00:00+00:00".to_string(),
        )]
        .into();
        let folded = fold_verification(&keyed(&current), &existing, &stamps, &[], "ns");
        assert_eq!(folded.entries.len(), 1);
        assert_eq!(folded.entries[0].repository.name, "nas");
        assert_eq!(
            folded.flat.as_deref(),
            Some("2026-08-02T00:00:00+00:00"),
            "the removed repo's ancient timestamp must not pin the MIN"
        );
    }

    // --- work-spec mapping (tier → VerifyOp) ---

    fn sample_policy(verification: Verification) -> SnapshotPolicy {
        use kopiur_api::snapshot_policy::{PvcSource, Source};
        SnapshotPolicy::new(
            "pg",
            kopiur_api::SnapshotPolicySpec {
                repository: Some(kopiur_api::common::RepositoryRef {
                    kind: Default::default(),
                    name: "r".into(),
                    namespace: None,
                }),
                repositories: vec![],
                identity: None,
                sources: vec![Source {
                    pvc: Some(PvcSource {
                        name: "data".into(),
                    }),
                    pvc_selector: None,
                    nfs: None,
                    source_path_override: None,
                    source_path_strategy: None,
                    ..Default::default()
                }],
                copy_method: Default::default(),
                volume_snapshot_class_name: None,
                staging: None,
                group_by: None,
                retention: None,
                default_deletion_policy: None,
                compression: None,
                files: None,
                extra_args: vec![],
                error_handling: None,
                upload: None,
                verification: Some(verification),
                preflight: None,
                suspend: false,
                hooks: None,
                mover: None,
                credential_projection: None,
                deletion: None,
                adoption: None,
            },
        )
    }

    fn sample_repo() -> ResolvedRepository {
        use kopiur_api::backend::{Backend, FilesystemBackend};
        use kopiur_api::common::{Encryption, RepositoryMode, SecretKeyRef};
        ResolvedRepository {
            backend: Backend::Filesystem(FilesystemBackend {
                path: "/repo".into(),
                volume: None,
            }),
            encryption: Encryption {
                password_secret_ref: SecretKeyRef {
                    name: "s".into(),
                    namespace: None,
                    key: None,
                },
            },
            kind: kopiur_api::common::RepositoryKind::Repository,
            repo_namespace: Some("ns".into()),
            mover_defaults: None,
            identity_defaults: None,
            schedule_defaults: None,
            on_namespace_delete: Default::default(),
            credential_projection_allowed: false,
            owner_ref: Default::default(),
            mode: RepositoryMode::ReadWrite,
            deletion_protection: None,
            concurrency: None,
            mass_deletion_ack: None,
            catalog: None,
            ca_bundle_pem: None,
        }
    }

    #[test]
    fn quick_work_spec_carries_quick_tier_and_success_expr() {
        let mut v = verification(Some("0 4 * * *"), None);
        v.success_expr = Some("stats.errors == 0".into());
        v.verify_files_percent = Some(10);
        let policy = sample_policy(v.clone());
        let repo = sample_repo();
        let ws = build_verify_work_spec(
            &policy,
            &repo,
            "ns",
            "pg",
            &v,
            VerifyTierKind::Quick,
            None,
            None,
            None,
        )
        .expect("identity resolves");
        match &ws.operation {
            Operation::Verify(op) => {
                assert_eq!(op.success_expr.as_deref(), Some("stats.errors == 0"));
                match &op.tier {
                    VerifyTier::Quick(q) => assert_eq!(q.verify_files_percent, Some(10)),
                    other => panic!("expected quick tier, got {}", other.kind_str()),
                }
            }
            other => panic!("expected verify op, got {}", other.kind_str()),
        }
        // The identity carries the recipe's resolved source path (/pvc/data).
        assert_eq!(ws.identity.source_path, "/pvc/data");
        assert_eq!(ws.target_ref.kind, "SnapshotPolicy");
    }

    #[test]
    fn quick_work_spec_maps_tuning_knobs_not_hardcoded_none() {
        // M3 (issue #216 category sweep) regression: `verification.quick.{parallel,
        // fileParallelism,fileQueueLength,maxErrors}` are dormant plumbing without
        // this mapping — the workspec and kopia client already support them, but
        // `build_verify_work_spec` used to hardcode `max_errors: None, parallel:
        // None` (and never had the other two at all), silently dropping every value
        // a user set. This must fail on the pre-fix hardcoded-`None` mapping.
        let mut v = verification(Some("0 4 * * *"), None);
        v.quick = Some(QuickVerification {
            schedule: Some(CronSpec {
                cron: "0 4 * * *".into(),
                jitter: None,
                timezone: None,
            }),
            parallel: Some(2),
            file_parallelism: Some(4),
            file_queue_length: Some(100),
            max_errors: Some(1),
        });
        let policy = sample_policy(v.clone());
        let repo = sample_repo();
        let ws = build_verify_work_spec(
            &policy,
            &repo,
            "ns",
            "pg",
            &v,
            VerifyTierKind::Quick,
            None,
            None,
            None,
        )
        .expect("identity resolves");
        match &ws.operation {
            Operation::Verify(op) => match &op.tier {
                VerifyTier::Quick(q) => {
                    assert_eq!(q.parallel, Some(2), "parallel must reach the workspec");
                    assert_eq!(
                        q.file_parallelism,
                        Some(4),
                        "fileParallelism must reach the workspec"
                    );
                    assert_eq!(
                        q.file_queue_length,
                        Some(100),
                        "fileQueueLength must reach the workspec"
                    );
                    assert_eq!(q.max_errors, Some(1), "maxErrors must reach the workspec");
                }
                other => panic!("expected quick tier, got {}", other.kind_str()),
            },
            other => panic!("expected verify op, got {}", other.kind_str()),
        }
    }

    #[test]
    fn deep_work_spec_carries_deep_tier_with_scratch_path() {
        let v = verification(None, Some("0 5 * * 0"));
        let policy = sample_policy(v.clone());
        let repo = sample_repo();
        let ws = build_verify_work_spec(
            &policy,
            &repo,
            "ns",
            "pg",
            &v,
            VerifyTierKind::Deep,
            None,
            None,
            None,
        )
        .expect("identity resolves");
        match &ws.operation {
            Operation::Verify(op) => match &op.tier {
                VerifyTier::Deep(d) => {
                    assert_eq!(d.scratch_path, DEEP_SCRATCH_PATH);
                    assert!(d.snapshot_id.is_none());
                }
                other => panic!("expected deep tier, got {}", other.kind_str()),
            },
            other => panic!("expected verify op, got {}", other.kind_str()),
        }
    }

    #[test]
    fn deep_work_spec_maps_parallel_not_hardcoded_none() {
        // M3 regression, deep side: `verification.deep.parallel` must reach
        // `DeepVerify.parallel` (the mover maps it into `restore --parallel`).
        let mut v = verification(None, Some("0 5 * * 0"));
        v.deep = Some(DeepVerification {
            schedule: CronSpec {
                cron: "0 5 * * 0".into(),
                jitter: None,
                timezone: None,
            },
            storage_class_name: None,
            capacity: None,
            parallel: Some(2),
        });
        let policy = sample_policy(v.clone());
        let repo = sample_repo();
        let ws = build_verify_work_spec(
            &policy,
            &repo,
            "ns",
            "pg",
            &v,
            VerifyTierKind::Deep,
            None,
            None,
            None,
        )
        .expect("identity resolves");
        match &ws.operation {
            Operation::Verify(op) => match &op.tier {
                VerifyTier::Deep(d) => {
                    assert_eq!(d.parallel, Some(2), "parallel must reach the workspec");
                }
                other => panic!("expected deep tier, got {}", other.kind_str()),
            },
            other => panic!("expected verify op, got {}", other.kind_str()),
        }
    }

    // --- scratch volume resolution (capacity gates emptyDir vs ephemeral PVC) ---
    // The repo↔recipe merge itself is covered in `crate::cache` tests; here we test
    // the capacity gate over the already-merged effective `ScratchDefaults`.

    fn scratch(capacity: Option<&str>, storage_class: Option<&str>) -> ScratchDefaults {
        ScratchDefaults {
            storage_class_name: storage_class.map(Into::into),
            capacity: capacity.map(Into::into),
        }
    }

    #[test]
    fn scratch_volume_defaults_to_emptydir_when_capacity_unset() {
        // Zero-config: the common "just enable deep verify" case. An emptyDir is
        // writable by the non-root mover (like the kopia cache), fixing the
        // original `mkdir /scratch: permission denied`. Also covers no effective
        // config at all (None).
        assert_eq!(scratch_volume(None), CacheVolume::EmptyDir);
        assert_eq!(
            scratch_volume(Some(&scratch(None, None))),
            CacheVolume::EmptyDir
        );
    }

    #[test]
    fn scratch_volume_storageclass_without_capacity_is_still_emptydir() {
        // Mirrors resolve_cache_volume: capacity is the gate. storageClassName has
        // no effect on an emptyDir, so without capacity we stay emptyDir.
        assert_eq!(
            scratch_volume(Some(&scratch(None, Some("openebs-hostpath")))),
            CacheVolume::EmptyDir
        );
    }

    #[test]
    fn scratch_volume_with_capacity_is_a_sized_ephemeral_volume() {
        assert_eq!(
            scratch_volume(Some(&scratch(Some("50Gi"), Some("fast-ssd")))),
            CacheVolume::Ephemeral {
                capacity: "50Gi".into(),
                storage_class: Some("fast-ssd".into()),
            }
        );
        // capacity alone → cluster default storage class.
        assert_eq!(
            scratch_volume(Some(&scratch(Some("10Gi"), None))),
            CacheVolume::Ephemeral {
                capacity: "10Gi".into(),
                storage_class: None,
            }
        );
    }

    fn deep_verification(storage_class: Option<&str>, capacity: Option<&str>) -> Verification {
        Verification {
            quick: None,
            deep: Some(DeepVerification {
                schedule: CronSpec {
                    cron: "0 5 * * 0".into(),
                    jitter: None,
                    timezone: None,
                },
                storage_class_name: storage_class.map(Into::into),
                capacity: capacity.map(Into::into),
                parallel: None,
            }),
            success_expr: None,
            verify_files_percent: None,
        }
    }

    #[test]
    fn scratch_storage_class_state_flags_only_storageclass_without_capacity() {
        let repo = sample_repo(); // no moverDefaults.scratch

        // No verification.deep at all → no condition to surface.
        assert!(
            scratch_storage_class_state(&repo, &verification(Some("0 4 * * *"), None)).is_none()
        );

        // Zero-config deep (emptyDir) → honored (not ignored).
        assert!(
            !scratch_storage_class_state(&repo, &deep_verification(None, None))
                .unwrap()
                .ignored
        );

        // storageClass without capacity → ignored (the no-op the user must fix).
        let st =
            scratch_storage_class_state(&repo, &deep_verification(Some("fast-ssd"), None)).unwrap();
        assert!(st.ignored);
        assert!(st.message.contains("fast-ssd"));
        assert!(st.message.contains("capacity"));

        // storageClass + capacity → honored.
        assert!(
            !scratch_storage_class_state(&repo, &deep_verification(Some("fast-ssd"), Some("50Gi")))
                .unwrap()
                .ignored
        );
    }

    #[test]
    fn scratch_storage_class_state_honors_repo_supplied_capacity() {
        // Repo-level moverDefaults.scratch.capacity makes a policy-only storageClass
        // NOT a no-op — the merged result has a capacity. (Guards the webhook
        // false-positive we deliberately avoid by validating post-merge.)
        let mut repo = sample_repo();
        repo.mover_defaults = Some(kopiur_api::common::MoverDefaults {
            scratch: Some(ScratchDefaults {
                storage_class_name: None,
                capacity: Some("100Gi".into()),
            }),
            ..Default::default()
        });
        let st =
            scratch_storage_class_state(&repo, &deep_verification(Some("fast-ssd"), None)).unwrap();
        assert!(
            !st.ignored,
            "repo-supplied capacity should make the SC honored"
        );
    }

    // --- #456: the (repository x member) verify fan-out -----------------------

    /// A policy built the way the apiserver hands one over: a JSON value (what
    /// a decoded CR body is) into the typed struct. Never `serde_yaml` straight
    /// into a typed value — 0.9 mis-encodes externally-tagged enums.
    fn selector_policy(sources: serde_json::Value) -> SnapshotPolicy {
        policy_json(serde_json::json!({
            "repository": { "name": "r" },
            "sources": sources,
            "verification": { "quick": { "schedule": { "cron": "*/5 * * * *" } } },
        }))
    }

    /// See [`selector_policy`].
    fn policy_json(spec: serde_json::Value) -> SnapshotPolicy {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotPolicy",
            "metadata": { "name": "pg", "namespace": "ns" },
            "spec": spec,
        }))
        .expect("typed SnapshotPolicy")
    }

    fn matched(
        pairs: &[(usize, &[(&str, &str)])],
    ) -> BTreeMap<usize, Vec<kopiur_api::snapshot::PvcTargetRef>> {
        pairs
            .iter()
            .map(|(index, pvcs)| {
                (
                    *index,
                    pvcs.iter()
                        .map(|(ns, name)| kopiur_api::snapshot::PvcTargetRef {
                            namespace: (*ns).to_string(),
                            name: (*name).to_string(),
                        })
                        .collect(),
                )
            })
            .collect()
    }

    fn paths(members: &[VerifyMember]) -> Vec<String> {
        members
            .iter()
            .map(|m| m.source_path.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn selector_policy_fans_out_one_member_per_matched_pvc_under_both_strategies() {
        // THE #456 bug: one member with an EMPTY path. Every member must carry
        // the DERIVED path its backup was written under, and under the strategy
        // that source declares.
        for (strategy, want) in [
            ("PvcName", vec!["/pvc/data-a", "/pvc/data-b"]),
            (
                "PvcNamespacedName",
                vec!["/pvc/ns/data-a", "/pvc/ns/data-b"],
            ),
        ] {
            let policy = selector_policy(serde_json::json!([{
                "pvcSelector": { "labelSelector": { "matchLabels": { "app": "web" } } },
                "sourcePathStrategy": strategy,
            }]));
            let members = verify_members(
                &policy,
                &matched(&[(0, &[("ns", "data-a"), ("ns", "data-b")])]),
            );
            assert_eq!(paths(&members), want, "strategy {strategy}");
            assert!(
                members.iter().all(|m| m.member6.is_some()),
                "a 2-member fan-out must carry the member dimension"
            );
            assert_ne!(
                members[0].member6, members[1].member6,
                "distinct paths must get distinct member tags, or two members share one \
                 Job name and one stamp"
            );
        }
    }

    #[test]
    fn verify_members_mirror_exactly_what_the_backup_side_mints() {
        // The invariant the whole fix rests on: verification must derive the
        // SAME set of kopia source paths `expand_sources` gives the backup. A
        // mixed `pvc` + `pvcSelector` policy backs up ONLY the selector members
        // — returning the union here would derive a path with no snapshots and
        // fail the whole policy.
        use kopiur_api::expand::{effective_source, expand_sources, strategy_for};
        let policy = selector_policy(serde_json::json!([
            { "pvc": { "name": "legacy" } },
            {
                "pvcSelector": { "labelSelector": { "matchLabels": { "app": "web" } } },
                "sourcePathStrategy": "PvcName",
            },
        ]));
        let m = matched(&[(1, &[("ns", "data-a"), ("ns", "data-b")])]);
        let backup: Vec<String> = expand_sources(&policy, "pg-1", &m)
            .expect("expansion")
            .expect("a selector policy expands")
            .iter()
            .map(|em| {
                let eff = effective_source(&policy, Some(&em.source)).expect("effective");
                eff.kopia_source_path(strategy_for(&policy.spec.sources[eff.index]))
                    .expect("a PVC member always has a path")
            })
            .collect();
        assert_eq!(
            paths(&verify_members(&policy, &m)),
            backup,
            "the verify member paths must equal the backup member paths, exactly"
        );
        assert!(
            !backup.iter().any(|p| p == "/pvc/legacy"),
            "sanity: expand_sources skips the plain pvc source, so verification must too"
        );
    }

    #[test]
    fn a_selector_matching_nothing_yields_no_members_so_nothing_is_spawned_or_stamped() {
        let policy = selector_policy(serde_json::json!([{
            "pvcSelector": { "labelSelector": { "matchLabels": { "app": "web" } } },
        }]));
        assert!(
            verify_members(&policy, &matched(&[(0, &[])])).is_empty(),
            "mirror SlotMintPlan::NothingMatched — never a pathless run that false-passes"
        );
        assert!(
            verify_members(&policy, &BTreeMap::new()).is_empty(),
            "an absent match entry is the same situation as an empty one"
        );
    }

    #[test]
    fn non_selector_policies_keep_the_pre_fix_shape_but_one_pvc_selectors_do_not() {
        // A plain `pvc:`/`nfs:` policy: ONE member, no path of its own (the
        // identity kernel derives it from the governing source exactly as
        // before) and no member dimension.
        for sources in [
            serde_json::json!([{ "pvc": { "name": "data" } }]),
            serde_json::json!([{ "nfs": { "server": "h", "path": "/export" } }]),
            serde_json::json!([]),
        ] {
            let members = verify_members(&selector_policy(sources.clone()), &BTreeMap::new());
            assert_eq!(members.len(), 1, "{sources}");
            assert_eq!(members[0].source_path, None, "{sources}");
            assert_eq!(members[0].member6, None, "{sources}");
        }
        // A selector matching exactly ONE PVC is NOT collapsed to the flat
        // shape: it needs the derived path (that is the bug), and it needs
        // stamps-map mode so the pre-#456 false-pass `lastVerified` — which
        // also ANCHORS the next due slot — is cleared instead of deferring the
        // first real verification by up to a full deep period.
        let one = selector_policy(serde_json::json!([{
            "pvcSelector": { "labelSelector": { "matchLabels": { "app": "web" } } },
        }]));
        let members = verify_members(&one, &matched(&[(0, &[("ns", "only")])]));
        assert_eq!(paths(&members), vec!["/pvc/only"]);
        assert_eq!(
            members[0].member6.as_deref(),
            Some(member_tag6("/pvc/only").as_str()),
            "a one-PVC selector keeps its member dimension"
        );
    }

    #[test]
    fn a_one_pvc_selector_with_an_override_verifies_the_override_path() {
        // #456 review: this combination is a footgun but NOT refused at
        // admission, because with exactly one matched PVC it works — the
        // override wins, there is no collision for `expand_sources` to refuse,
        // and real snapshots exist at that path. Verification must therefore
        // check THAT path, not `/pvc/<name>` and not an empty one.
        let policy = selector_policy(serde_json::json!([{
            "pvcSelector": { "labelSelector": { "matchLabels": { "app": "web" } } },
            "sourcePathOverride": "/data",
        }]));
        let members = verify_members(&policy, &matched(&[(0, &[("ns", "only")])]));
        assert_eq!(paths(&members), vec!["/data"]);
        // A SECOND matching PVC makes the BACKUP itself refuse (both would land
        // on `/data`), so no further snapshot is minted. Verification still
        // derives the one shared path once — and deliberately keeps PASSING,
        // because the snapshots taken before the second PVC appeared are still
        // there. The loud signal for this drift is the refused schedule fire,
        // not verification; verification only fails where the path never
        // received a backup at all.
        let two = verify_members(&policy, &matched(&[(0, &[("ns", "a"), ("ns", "b")])]));
        assert_eq!(paths(&two), vec!["/data"]);
    }

    #[test]
    fn two_selector_sources_on_one_path_verify_it_once_instead_of_twice() {
        // `expand_sources` REFUSES this (two backups would overwrite each
        // other); verification only reads, so it degrades to one run rather
        // than parking the policy on a fault the backup side already reports.
        let policy = selector_policy(serde_json::json!([
            { "pvcSelector": { "labelSelector": { "matchLabels": { "a": "1" } } } },
            { "pvcSelector": { "labelSelector": { "matchLabels": { "b": "2" } } } },
        ]));
        let members = verify_members(
            &policy,
            &matched(&[(0, &[("ns", "shared")]), (1, &[("ns", "shared")])]),
        );
        assert_eq!(paths(&members), vec!["/pvc/shared"]);
    }

    // --- identity: the derived path reaches the wire, errors are not masked ---

    #[test]
    fn member_path_lands_verbatim_on_the_work_spec_identity() {
        let policy = selector_policy(serde_json::json!([{
            "pvcSelector": { "labelSelector": { "matchLabels": { "app": "web" } } },
        }]));
        let v = policy.spec.verification.clone().expect("verification");
        let ws = build_verify_work_spec(
            &policy,
            &sample_repo(),
            "ns",
            "pg",
            &v,
            VerifyTierKind::Quick,
            None,
            Some("/pvc/data-b"),
            stamp_key(None, Some("abc123")),
        )
        .expect("identity resolves");
        assert_eq!(
            ws.identity.source_path, "/pvc/data-b",
            "the member path must be used VERBATIM — a re-derived /pvc/<first source> or an \
             empty path is #456"
        );
        assert_ne!(
            ws.identity.source_spec(),
            format!("{}@{}:", ws.identity.username, ws.identity.hostname),
            "a pathless `user@host:` source spec is what kopia silently matched zero \
             manifests for (quick verify exited 0 on a false pass)"
        );
        match &ws.operation {
            Operation::Verify(op) => assert_eq!(op.stamp_key.as_deref(), Some("#abc123")),
            other => panic!("expected a verify op, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_source_legacy_policy_keeps_its_pathless_identity_byte_for_byte() {
        let policy = selector_policy(serde_json::json!([]));
        let v = policy.spec.verification.clone().expect("verification");
        let ws = build_verify_work_spec(
            &policy,
            &sample_repo(),
            "ns",
            "pg",
            &v,
            VerifyTierKind::Quick,
            None,
            None,
            None,
        )
        .expect("identity resolves");
        assert_eq!(
            ws.identity.source_path, "",
            "a zero-source legacy policy has no path at all; inventing /data would break \
             a working verify on upgrade"
        );
    }

    #[test]
    fn an_unresolvable_cel_identity_parks_instead_of_verifying_a_sentinel() {
        // Documented behaviour change: this used to be masked as
        // `kopiur-verify@<ns>:` with an empty path — a run that verified
        // nothing and reported success.
        let policy = selector_policy(serde_json::json!([{ "pvc": { "name": "data" } }]));
        let v = policy.spec.verification.clone().expect("verification");
        // The CEL `*Expr` pair lives on the REPOSITORY's identityDefaults.
        let mut repo = sample_repo();
        repo.identity_defaults = Some(
            serde_json::from_value(serde_json::json!({ "usernameExpr": "namespace +" }))
                .expect("typed IdentityDefaults"),
        );
        let err = build_verify_work_spec(
            &policy,
            &repo,
            "ns",
            "pg",
            &v,
            VerifyTierKind::Quick,
            None,
            None,
            None,
        )
        .expect_err("an unresolvable identity must propagate, not fall back to a sentinel");
        assert!(
            matches!(err, crate::error::Error::Validation(_)),
            "expected a Validation error, got {err:?}"
        );
    }

    // --- seeds, names and labels ---------------------------------------------

    #[test]
    fn member_seeds_spread_the_slots_so_members_do_not_all_fire_at_once() {
        let spec = cron_spec("0 * * * *", Some("30m"));
        let after = jitter_after();
        let a = slot_for("uid|/pvc/data-a", &spec, after, None).expect("slot a");
        let b = slot_for("uid|/pvc/data-b", &spec, after, None).expect("slot b");
        assert_ne!(
            a, b,
            "two members must not share a jitter offset, or N quick verifies (or a \
             stampede of deep ones) fire in the same second"
        );
        // And the one-member shape keeps the bare-UID seed, so its slots are
        // byte-identical to a pre-#456 operator's.
        assert_eq!(
            slot_for("uid", &spec, after, None).expect("slot"),
            slot_for("uid", &spec, after, None).expect("slot"),
        );
    }

    #[test]
    fn verify_job_name_gains_m6_only_when_fanning_out_and_stays_within_52() {
        let slot = DateTime::parse_from_rfc3339("2026-06-09T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let unix = slot.timestamp();
        let m6 = member_tag6("/pvc/data-a");
        // Single-member: byte-identical legacy format (slot continuity).
        assert_eq!(
            verify_job_name("pg", VerifyTierKind::Quick, slot, None, None),
            format!("pg-vfy-q-{unix}")
        );
        assert_eq!(
            verify_job_name("pg", VerifyTierKind::Quick, slot, None, Some(&m6)),
            format!("pg-vfy-q-m{m6}-{unix}")
        );
        let r6 = crate::naming::repo_tag6("Repository/backups/nas");
        assert_eq!(
            verify_job_name("pg", VerifyTierKind::Quick, slot, Some(&r6), Some(&m6)),
            format!("pg-vfy-q-{r6}-m{m6}-{unix}")
        );
        // Sibling members never collide on a name.
        assert_ne!(
            verify_job_name("pg", VerifyTierKind::Quick, slot, Some(&r6), Some(&m6)),
            verify_job_name(
                "pg",
                VerifyTierKind::Quick,
                slot,
                Some(&r6),
                Some(&member_tag6("/pvc/data-b"))
            )
        );
        // MAX is 52, NOT 63: the remaining budget is the pod-name suffix.
        let long = "a-very-long-snapshot-policy-name-that-blows-the-dns-label-budget";
        let n = verify_job_name(long, VerifyTierKind::Deep, slot, Some(&r6), Some(&m6));
        assert!(n.len() <= 52, "{n} is {} bytes", n.len());
    }

    #[test]
    fn verify_member_label_is_group_prefixed_and_distinct() {
        assert!(VERIFY_MEMBER_LABEL.starts_with("kopiur.home-operations.com/"));
        assert_ne!(VERIFY_MEMBER_LABEL, VERIFY_REPO_LABEL);
        assert_ne!(VERIFY_MEMBER_LABEL, VERIFY_INSTANCE_LABEL);
    }

    // --- stamp keys -----------------------------------------------------------

    #[test]
    fn stamp_key_shapes_and_a_total_parser() {
        assert_eq!(stamp_key(None, None), None, "flat lastVerified");
        assert_eq!(
            stamp_key(Some("Repository/ns/nas"), None).as_deref(),
            Some("Repository/ns/nas"),
            "#368 wire, unchanged"
        );
        assert_eq!(
            stamp_key(None, Some("abc123")).as_deref(),
            Some("#abc123"),
            "single-repo fan-out: an EMPTY repo segment"
        );
        assert_eq!(
            stamp_key(Some("Repository/ns/nas"), Some("abc123")).as_deref(),
            Some("Repository/ns/nas#abc123")
        );
        // Total: every shape round-trips, and a garbage key never panics.
        assert_eq!(
            parse_stamp_key("Repository/ns/nas"),
            ("Repository/ns/nas", None)
        );
        assert_eq!(parse_stamp_key("#abc123"), ("", Some("abc123")));
        assert_eq!(
            parse_stamp_key("Repository/ns/nas#abc123"),
            ("Repository/ns/nas", Some("abc123"))
        );
        assert_eq!(parse_stamp_key(""), ("", None));
        assert_eq!(parse_stamp_key("#"), ("", Some("")));
        assert_eq!(parse_stamp_key("a#b#c"), ("a#b", Some("c")));
    }

    #[test]
    fn the_member_path_is_what_makes_the_discovered_unlock_match() {
        // WIRING guard for the #168 escape hatch (#456 review). The unlock
        // compares a member's resolved identity against what the catalog
        // recorded for a discovered/replicated row, and the catalog records the
        // PER-MEMBER kopia source path (`status.snapshot.identity.sourcePath`,
        // straight off the kopia manifest). So `run_verify_steps` must resolve
        // each member's identity with THAT member's own derived path.
        //
        // Passing `None` instead — the obvious-looking simplification — is
        // fail-CLOSED: the keys would systematically disagree, no adopted row
        // would ever match, and the escape hatch would be silently dead
        // forever with nothing to notice it. That is exactly the failure this
        // asserts against.
        let policy = selector_policy(serde_json::json!([{
            "pvcSelector": { "labelSelector": { "matchLabels": { "app": "web" } } },
            "sourcePathStrategy": "PvcName",
        }]));
        let members = verify_members(
            &policy,
            &matched(&[(0, &[("ns", "data-a"), ("ns", "data-b")])]),
        );
        assert_eq!(members.len(), 2);

        // What the catalog holds: one adopted row per member, recorded at the
        // member's own path under the policy's identity.
        let catalog: BTreeSet<IdentityKey> = members
            .iter()
            .map(|m| {
                identity_key(&kopiur_api::common::ResolvedIdentity {
                    username: "pg".into(),
                    hostname: "ns".into(),
                    source_path: m.source_path.clone(),
                })
            })
            .collect();
        assert_eq!(catalog.len(), 2, "two distinct recorded identities");

        // The composition `run_verify_steps` performs, per member.
        for m in &members {
            let resolved = crate::snapshot_policy::config_identity_for_path(
                &policy,
                "ns",
                None,
                m.source_path.as_deref(),
            )
            .expect("identity resolves");
            assert_eq!(
                resolved.source_path, m.source_path,
                "the member's own path must ride through to the identity verbatim"
            );
            assert!(
                catalog.contains(&identity_key(&resolved)),
                "member {:?} must match its own adopted row",
                m.source_path
            );
        }

        // And the negative: resolving with `None` produces ONE address for both
        // members that matches NEITHER row, so the hatch would never open.
        let pathless = crate::snapshot_policy::config_identity_for_path(&policy, "ns", None, None)
            .expect("identity resolves");
        assert!(
            !catalog.contains(&identity_key(&pathless)),
            "passing None would silently kill the escape hatch: {:?} vs {catalog:?}",
            identity_key(&pathless)
        );
    }

    #[test]
    fn the_catalog_probe_asks_only_about_gated_members_and_is_skipped_when_none_are() {
        let key = |p: &str| {
            identity_key(&kopiur_api::common::ResolvedIdentity {
                username: "pg".into(),
                hostname: "ns".into(),
                source_path: Some(p.to_string()),
            })
        };
        let a = key("/pvc/data-a");
        let b = key("/pvc/data-b");

        // Every member already has its own successful backup: no adopted row
        // could change any outcome, so the paged LIST is skipped entirely (an
        // empty wanted set short-circuits `discovered_identity_index` before
        // any request).
        assert!(
            probe_identities(&[(true, a.clone()), (true, b.clone())]).is_empty(),
            "an all-verified repository must not be re-read every reconcile"
        );

        // Only the gated member is asked about — which is also what lets
        // `discovered_identities` drop every irrelevant row and stop as soon as
        // it has them all.
        assert_eq!(
            probe_identities(&[(true, a.clone()), (false, b.clone())]),
            std::iter::once(b.clone()).collect::<BTreeSet<_>>()
        );
        assert_eq!(
            probe_identities(&[(false, a.clone()), (false, b.clone())]),
            [a, b].into_iter().collect::<BTreeSet<_>>()
        );
        assert!(probe_identities(&[]).is_empty());
    }

    #[test]
    fn the_discovered_unlock_is_keyed_on_identity_not_merely_on_the_repository() {
        // #456 review: the escape hatch used to be "this repository holds SOME
        // discovered/replicated row", with no identity filter. That is the
        // ORDINARY state of a new policy on a shared repository (see
        // `AdoptionSummary::last_scan_unmatched`), so it unlocked verification
        // for an identity with no manifest at all — and once a quick verify
        // that covered nothing became terminal, that meant a FAILED verify Job
        // every slot until the first own backup landed.
        let id = |u: &str, h: &str, p: Option<&str>| kopiur_api::common::ResolvedIdentity {
            username: u.to_string(),
            hostname: h.to_string(),
            source_path: p.map(str::to_string),
        };
        // What a shared repository's catalog looks like to a brand-new policy:
        // plenty of rows, none of them this member's.
        let foreign: BTreeSet<IdentityKey> = [
            id("other", "ns", Some("/pvc/theirs")),
            id("pg", "otherns", Some("/pvc/data-a")),
            id("pg", "ns", Some("/pvc/data-z")),
        ]
        .iter()
        .map(identity_key)
        .collect();
        let mine = identity_key(&id("pg", "ns", Some("/pvc/data-a")));
        assert!(
            !foreign.contains(&mine),
            "a foreign catalog must NOT unlock this member"
        );
        assert!(
            !verification_unlocked(false, foreign.contains(&mine)),
            "so the gate stays closed until this member's own first backup"
        );
        // An adopted repository that really does hold THIS identity still
        // unlocks — the escape hatch #168 added must keep working.
        let adopted: BTreeSet<IdentityKey> = std::iter::once(mine.clone()).collect();
        assert!(verification_unlocked(false, adopted.contains(&mine)));

        // All three components matter, and `None` is NOT `Some("")`: a pathless
        // identity is a different kopia address from one with an empty path.
        for other in [
            id("pg", "ns", None),
            id("pg", "ns", Some("")),
            id("pg", "ns2", Some("/pvc/data-a")),
            id("pg2", "ns", Some("/pvc/data-a")),
        ] {
            assert_ne!(
                identity_key(&other),
                mine,
                "{other:?} must not be treated as the same identity"
            );
        }
    }

    #[test]
    fn repo_keys_are_hash_free_so_the_separator_is_unambiguous() {
        // `parse_stamp_key` splits on the LAST '#'. That is only total because a
        // repository key cannot contain one. Asserted over REAL `repo_key`
        // output (not a hand-written string) across every ref shape, plus the
        // member tag's own alphabet, so a future key format that admitted '#'
        // fails here instead of silently pruning a live stamp every reconcile.
        for r in [
            rref("Repository", Some("ns"), "nas"),
            rref("Repository", None, "nas"),
            rref("ClusterRepository", None, "offsite"),
            rref("ClusterRepository", Some("ignored"), "offsite"),
        ] {
            let key = kopiur_api::common::repo_key(&r, "ns");
            assert!(!key.contains('#'), "repo key {key:?} must be '#'-free");
            // And it round-trips through the pair unchanged.
            let stamp = stamp_key(Some(&key), Some("abc123")).expect("keyed");
            assert_eq!(parse_stamp_key(&stamp), (key.as_str(), Some("abc123")));
        }
        let tag = member_tag6("/pvc/data#weird");
        assert!(
            tag.chars().all(|c| c.is_ascii_hexdigit()),
            "the member tag must stay hex ({tag}) — it is the segment AFTER the separator"
        );
    }

    #[test]
    fn the_prune_keeps_member_keys_and_drops_only_dead_cells() {
        // The #368 prune kept a key only when it equalled a current REPO key
        // exactly, so it deleted every member-keyed stamp on the next
        // reconcile — `lastVerified` never advanced and a due deep verify
        // re-fired every slot forever.
        let repos: BTreeSet<&str> = ["Repository/ns/a", "Repository/ns/b"].into_iter().collect();
        let members: BTreeSet<&str> = ["m1", "m2"].into_iter().collect();
        for live in [
            "Repository/ns/a#m1",
            "Repository/ns/a#m2",
            "Repository/ns/b#m1",
        ] {
            assert!(
                stamp_key_live(live, &repos, &members),
                "{live} must survive"
            );
        }
        for dead in [
            "Repository/ns/a",      // bare key, but the policy fans out now
            "Repository/ns/c#m1",   // repository left the spec
            "Repository/ns/a#gone", // PVC no longer matches
        ] {
            assert!(
                !stamp_key_live(dead, &repos, &members),
                "{dead} must be pruned"
            );
        }
        // Single-repo fan-out: the repo segment is the empty string.
        let single: BTreeSet<&str> = [""].into_iter().collect();
        assert!(stamp_key_live("#m1", &single, &members));
        assert!(!stamp_key_live("#m9", &single, &members));
        // No member dimension: the bare repo key is the live shape, and a
        // leftover member key is dead.
        let none: BTreeSet<&str> = BTreeSet::new();
        assert!(stamp_key_live("Repository/ns/a", &repos, &none));
        assert!(!stamp_key_live("Repository/ns/a#m1", &repos, &none));
    }

    #[test]
    fn fold_takes_the_min_over_members_so_a_partial_repo_is_never_fully_verified() {
        let repos = [rref("Repository", Some("ns"), "a")];
        let members = vec!["m1".to_string(), "m2".to_string()];
        let mut stamps = BTreeMap::new();
        stamps.insert(
            "Repository/ns/a#m1".to_string(),
            "2026-08-02T00:00:00+00:00".to_string(),
        );
        // Only ONE of two members has verified.
        let folded = fold_verification(&keyed(&repos), &[], &stamps, &members, "ns");
        assert_eq!(
            folded.entries[0].last_verified, None,
            "a repository whose members are only PARTLY verified must not look verified"
        );
        assert_eq!(folded.flat, None);
        // Both members: the entry (and the flat field) is the MIN, i.e. the age
        // of the STALEST member — "everything is verified as of T".
        stamps.insert(
            "Repository/ns/a#m2".to_string(),
            "2026-08-05T00:00:00+00:00".to_string(),
        );
        let folded = fold_verification(&keyed(&repos), &[], &stamps, &members, "ns");
        assert_eq!(
            folded.entries[0].last_verified.as_deref(),
            Some("2026-08-02T00:00:00+00:00")
        );
        assert_eq!(folded.flat.as_deref(), Some("2026-08-02T00:00:00+00:00"));
    }

    #[test]
    fn fold_flat_is_the_min_over_every_repo_times_member_cell() {
        let repos = [
            rref("Repository", Some("ns"), "a"),
            rref("ClusterRepository", None, "b"),
        ];
        let members = vec!["m1".to_string(), "m2".to_string()];
        let stamps: BTreeMap<String, String> = [
            ("Repository/ns/a#m1", "2026-08-09T00:00:00+00:00"),
            ("Repository/ns/a#m2", "2026-08-08T00:00:00+00:00"),
            ("ClusterRepository/b#m1", "2026-08-03T00:00:00+00:00"),
            ("ClusterRepository/b#m2", "2026-08-07T00:00:00+00:00"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let folded = fold_verification(&keyed(&repos), &[], &stamps, &members, "ns");
        assert_eq!(
            folded.entries[0].last_verified.as_deref(),
            Some("2026-08-08T00:00:00+00:00")
        );
        assert_eq!(
            folded.entries[1].last_verified.as_deref(),
            Some("2026-08-03T00:00:00+00:00")
        );
        assert_eq!(
            folded.flat.as_deref(),
            Some("2026-08-03T00:00:00+00:00"),
            "the flat field is the MIN over all (repo x member) cells"
        );
    }

    // --- single-flight across an upgrade -------------------------------------

    fn verify_job(name: &str, member6: Option<&str>) -> Job {
        let mut labels = BTreeMap::new();
        labels.insert(COMPONENT_LABEL.to_string(), VERIFY_COMPONENT.to_string());
        labels.insert(VERIFY_INSTANCE_LABEL.to_string(), "pg".to_string());
        if let Some(m6) = member6 {
            labels.insert(VERIFY_MEMBER_LABEL.to_string(), m6.to_string());
        }
        Job {
            metadata: kube::core::ObjectMeta {
                name: Some(name.to_string()),
                labels: Some(labels),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn the_flight_selector_never_narrows_by_member() {
        // THE invariant of the upgrade guard, asserted rather than left to a
        // reviewer noticing a one-line change. Putting VERIFY_MEMBER_LABEL in
        // the LIST makes a Job minted by an older operator (which carries
        // neither the member nor the repo label) invisible, and the reconciler
        // then spawns one fresh Job per member beside it — for the deep tier,
        // N+1 concurrent scratch restores. A regression here is silent.
        for repo6 in [None, Some("abc123")] {
            let selector = verify_flight_selector("pg", repo6);
            assert!(
                !selector.contains(VERIFY_MEMBER_LABEL),
                "the single-flight LIST must never narrow by member: {selector}"
            );
            assert!(selector.contains(&format!("{COMPONENT_LABEL}={VERIFY_COMPONENT}")));
            assert!(selector.contains(&format!("{VERIFY_INSTANCE_LABEL}=pg")));
        }
        // The repository scope IS part of the LIST (repo A's verify must not
        // block repo B's), and absent for the single-repo shape so in-flight
        // Jobs from operators predating that label still match.
        assert!(
            verify_flight_selector("pg", Some("abc123"))
                .contains(&format!("{VERIFY_REPO_LABEL}=abc123"))
        );
        assert!(!verify_flight_selector("pg", None).contains(VERIFY_REPO_LABEL));
    }

    #[test]
    fn job_tier_reads_the_tier_off_every_name_shape_verify_job_name_produces() {
        let slot = DateTime::parse_from_rfc3339("2026-06-09T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let r6 = crate::naming::repo_tag6("Repository/backups/nas");
        let m6 = member_tag6("/pvc/data-a");
        for (repo, member) in [
            (None, None),
            (None, Some(m6.as_str())),
            (Some(r6.as_str()), None),
            (Some(r6.as_str()), Some(m6.as_str())),
        ] {
            for tier in [VerifyTierKind::Quick, VerifyTierKind::Deep] {
                // A policy name that itself contains the marker: the LAST
                // occurrence is the authoritative one.
                for policy in ["pg", "app-vfy-q-1"] {
                    let name = verify_job_name(policy, tier, slot, repo, member);
                    assert_eq!(
                        job_tier(&verify_job(&name, member)),
                        Some(tier),
                        "could not read the tier off {name}"
                    );
                }
            }
        }
        // An unreadable name is "cannot tell", which must block (there is no
        // tier label to add retroactively).
        assert_eq!(job_tier(&verify_job("hand-renamed", None)), None);
        assert!(job_blocks_cell(
            &verify_job("hand-renamed", Some("m1")),
            VerifyTierKind::Deep,
            Some("m2")
        ));
    }

    #[test]
    fn a_legacy_unlabelled_verify_job_blocks_every_cell() {
        // THE upgrade hazard. An in-flight Job minted by an operator predating
        // the member label carries neither VERIFY_MEMBER_LABEL nor
        // VERIFY_REPO_LABEL. It is found because the LIST does not narrow by
        // member, and it must hold every cell's slot.
        let legacy = verify_job("pg-vfy-d-1780000000", None);
        for tier in [VerifyTierKind::Quick, VerifyTierKind::Deep] {
            for member in [None, Some("m1"), Some("m2")] {
                assert!(
                    job_blocks_cell(&legacy, tier, member),
                    "legacy Job must block {tier:?}/{member:?}"
                );
            }
        }
    }

    #[test]
    fn quick_members_run_concurrently_while_deep_members_run_one_at_a_time() {
        let quick_m1 = verify_job("pg-vfy-q-m111111-1780000000", Some("m111111"));
        let deep_m1 = verify_job("pg-vfy-d-m111111-1780000000", Some("m111111"));

        // Quick is per-member.
        assert!(job_blocks_cell(
            &quick_m1,
            VerifyTierKind::Quick,
            Some("m111111")
        ));
        assert!(
            !job_blocks_cell(&quick_m1, VerifyTierKind::Quick, Some("m222222")),
            "sibling quick members are independent short reads and run concurrently"
        );
        assert!(
            job_blocks_cell(&quick_m1, VerifyTierKind::Quick, None),
            "while NOT fanning out, any labelled Job holds the slot"
        );

        // Deep is sequential across members — but ONLY against other deep
        // Jobs. Blocking it on an unrelated member's quick run starves the
        // drill on a wide policy (the odds some quick Job is live at deep-due
        // time scale with N, and a held deep suppresses that member's quick
        // too, since due_tier prefers deep).
        assert!(job_blocks_cell(
            &deep_m1,
            VerifyTierKind::Deep,
            Some("m222222")
        ));
        assert!(job_blocks_cell(
            &deep_m1,
            VerifyTierKind::Deep,
            Some("m111111")
        ));
        assert!(
            !job_blocks_cell(&quick_m1, VerifyTierKind::Deep, Some("m222222")),
            "an unrelated member's QUICK Job must not defer the deep drill"
        );
        assert!(
            !job_blocks_cell(&deep_m1, VerifyTierKind::Quick, Some("m222222")),
            "and a deep drill must not block a sibling's quick verify either"
        );
    }
}
