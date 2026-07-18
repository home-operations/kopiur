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

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use k8s_openapi::api::batch::v1::Job;
use kube::api::ListParams;
use kube::{Api, ResourceExt};

use kopiur_api::common::{CronSpec, ScratchDefaults};
use kopiur_api::{Origin, Snapshot, SnapshotPolicy, Verification};
use kopiur_mover::workspec::{
    DeepVerify, MoverOptions, MoverWorkSpec, Operation, QuickVerify, ResolvedIdentity, TargetRef,
    VerifyOp, VerifyTier,
};

use crate::consts::{
    API_VERSION, COMPONENT_LABEL, ORIGIN_LABEL, REPOSITORY_UID_LABEL, VERIFY_COMPONENT,
    VERIFY_INSTANCE_LABEL, VERIFY_SLOT_ANNOTATION,
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
/// `timezone` (absent ⇒ the repository's `scheduleDefaults.timezone`, else UTC).
fn slot_for(
    seed: &str,
    spec: &CronSpec,
    after: DateTime<Utc>,
    repo_tz: Option<&str>,
) -> Result<DateTime<Utc>> {
    let jitter = spec.jitter.as_deref().and_then(parse_go_duration);
    let tz = kopiur_api::common::resolve_tz_with_default(spec.timezone.as_deref(), repo_tz);
    next_fire(&spec.cron, jitter, seed, after, tz)
}

/// Whether verification is *unlocked*: a snapshot actually exists to verify. Pure.
///
/// The `tier_after` catch-up (a "never verified" policy anchors a year ago so the
/// first slot is immediately past-due) is deliberate for missed slots — but on a
/// brand-new policy it would fire a verify Job *before any backup exists*, and the
/// mover fails hard (`deep verify found no snapshot to restore …`, #168). Gate it:
/// schedule no verify until either this policy has produced a successful backup
/// (`has_successful`) OR its resolved repository already contains **discovered**
/// (adopted) snapshots (`has_discovered`) — the adopted-repo escape hatch, where
/// deep verify legitimately resolves the latest repo snapshot for the identity.
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

/// **Pure.** Decide where [`repo_has_discovered_snapshots`] should LIST, from the
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

/// Decide which verification tier is due now, preferring deep (it subsumes quick).
/// Returns the tier + its scheduled slot, or `None` if nothing is due. Pure given
/// the policy's `verification`, the seed, the last-verified time, `now`, the
/// repository's `scheduleDefaults.timezone` (`repo_tz`, GitHub #174 item 3), and
/// whether verification is `unlocked` ([`verification_unlocked`]) — a locked policy
/// is never due (the #168 gate), so the catch-up slot is not consumed until a
/// snapshot exists.
pub fn due_tier(
    verification: &Verification,
    seed: &str,
    last_verified: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    repo_tz: Option<&str>,
    unlocked: bool,
) -> Option<(VerifyTierKind, DateTime<Utc>)> {
    if !unlocked {
        return None;
    }
    let after = tier_after(last_verified);
    if let Some(d) = &verification.deep
        && let Ok(slot) = slot_for(seed, &d.schedule, after, repo_tz)
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
        && let Ok(slot) = slot_for(seed, schedule, after, repo_tz)
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
    repo_tz: Option<&str>,
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
        if let Ok(slot) = slot_for(seed, spec, after, repo_tz) {
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
    repo_tz: Option<&str>,
    unlocked: bool,
) -> Duration {
    if unlocked {
        next_verify_wakeup(verification, seed, last_verified, now, repo_tz).min(REQUEUE_CAP)
    } else {
        REQUEUE_GATED
    }
}

/// Deterministic, ≤52-char, DNS-1123-safe per-slot verify Job name:
/// `<policy>-vfy-<q|d>-<unix_slot>` (truncate+hash long policy names, like
/// maintenance).
fn verify_job_name(policy: &str, tier: VerifyTierKind, slot: DateTime<Utc>) -> String {
    const MAX: usize = 52;
    let suffix = format!("-vfy-{}-{}", tier.tag(), slot.timestamp());
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

/// The verification scheduling step, called by the `SnapshotPolicy` reconciler
/// (after it has confirmed the policy is non-suspended and its Repository is Ready).
/// Returns `Some(requeue)` when verification is configured (so the caller honors the
/// verify cadence), or `None` when `spec.verification` is absent (no behavior change).
pub async fn verify_step(
    config: &SnapshotPolicy,
    ctx: &Context,
    repo: &ResolvedRepository,
    namespace: &str,
    has_successful: bool,
) -> Result<Option<Duration>> {
    let Some(verification) = config.spec.verification.as_ref() else {
        return Ok(None);
    };
    let name = config.name_any();
    let seed = config.uid().unwrap_or_else(|| name.clone());
    let now = Utc::now();
    let last_verified = config
        .status
        .as_ref()
        .and_then(|s| s.last_verified.as_deref())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));
    // GitHub #174 item 3: a per-cron `timezone` wins; else the repository's
    // `scheduleDefaults.timezone`; else UTC.
    let repo_tz = repo
        .schedule_defaults
        .as_ref()
        .and_then(|d| d.timezone.as_deref());

    // #168 gate: never schedule a verify before a verifiable snapshot exists — the
    // tier_after catch-up would otherwise fire a Job on the first reconcile, before
    // any backup, and the mover fails hard. Unlocked once this policy has a Succeeded
    // snapshot OR its repo already carries discovered (adopted) snapshots. The
    // discovered probe is a cheap labeled LIST (thin IO over the pure gate) — skipped
    // entirely once a successful backup already unlocks the gate. Probed in the
    // repository's OWN namespace (not the policy's — `RepositoryRef` allows a
    // cross-namespace reference), or cluster-wide for a cluster-scoped repository
    // (`repo_namespace == None`), which scatters its discovered rows across
    // per-identity namespaces.
    let has_discovered = if has_successful {
        false
    } else {
        let scope = discovered_probe_scope(repo.repo_namespace.as_deref());
        repo_has_discovered_snapshots(&ctx.client, &scope, &repo.owner_ref.uid).await?
    };
    let unlocked = verification_unlocked(has_successful, has_discovered);

    let Some((tier, slot)) = due_tier(verification, &seed, last_verified, now, repo_tz, unlocked)
    else {
        if !unlocked {
            tracing::debug!(
                policy = %name,
                "verification gated: no verifiable snapshot yet (no successful backup and no \
                 discovered snapshots); deferring until the first successful backup"
            );
        }
        return Ok(Some(idle_requeue(
            verification,
            &seed,
            last_verified,
            now,
            repo_tz,
            unlocked,
        )));
    };

    let job_name = verify_job_name(&name, tier, slot);
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    match job_api.get_opt(&job_name).await? {
        Some(job) => match job_terminal_state(&job) {
            // Success: the mover stamped lastVerified; sleep until the next slot.
            // The terminal Job (its env carries the whole run spec) self-reaps
            // via its TTL — it is the slot-handled marker until then.
            Some(true) => Ok(Some(
                next_verify_wakeup(verification, &seed, Some(now), now, repo_tz).min(REQUEUE_CAP),
            )),
            // Failure: the failed Job lingers to its TTL as the bounded-retry
            // backoff (and keeps the pod logs).
            Some(false) => Ok(Some(REQUEUE_FAILED)),
            None => Ok(Some(REQUEUE_RUNNING)),
        },
        None => {
            if has_active_verify_job(&job_api, &name).await? {
                return Ok(Some(REQUEUE_RUNNING));
            }
            spawn_verify_job(
                config,
                ctx,
                repo,
                namespace,
                &name,
                &job_name,
                verification,
                tier,
                slot,
            )
            .await?;
            tracing::info!(policy = %name, ?tier, slot = %slot.to_rfc3339(), "spawned verification Job");
            Ok(Some(REQUEUE_RUNNING))
        }
    }
}

/// Build + apply the per-slot verification mover Job.
#[allow(clippy::too_many_arguments)]
async fn spawn_verify_job(
    config: &SnapshotPolicy,
    ctx: &Context,
    repo: &ResolvedRepository,
    namespace: &str,
    policy_name: &str,
    job_name: &str,
    verification: &Verification,
    tier: VerifyTierKind,
    slot: DateTime<Utc>,
) -> Result<()> {
    let work_spec =
        build_verify_work_spec(config, repo, namespace, policy_name, verification, tier);

    let mut labels = BTreeMap::new();
    labels.insert(COMPONENT_LABEL.to_string(), VERIFY_COMPONENT.to_string());
    labels.insert(VERIFY_INSTANCE_LABEL.to_string(), policy_name.to_string());
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

    let creds = io::resolve_mover_creds_for(
        &ctx.client,
        namespace,
        &io::CredsPrefix::verification(policy_name),
        &owner,
        repo,
        config
            .spec
            .credential_projection
            .as_ref()
            .is_some_and(|p| p.enabled),
        io::repo_kind_str(config.spec.repository.kind),
        &config.spec.repository.name,
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
        labels,
        source_volume: None,
        repo_volume,
        creds_secrets,
        result_configmap: None,
        service_account: mover_identity.service_account.as_deref(),
        passthrough_env: ctx.mover_env_passthrough.clone(),
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
pub fn build_verify_work_spec(
    config: &SnapshotPolicy,
    repo: &ResolvedRepository,
    namespace: &str,
    policy_name: &str,
    verification: &Verification,
    tier: VerifyTierKind,
) -> MoverWorkSpec {
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
    // The source identity (first source) so a deep restore targets the right path.
    let identity = verify_identity(config, namespace, repo);
    MoverWorkSpec {
        version: 1,
        operation: Operation::Verify(VerifyOp {
            tier,
            success_expr: verification.success_expr.clone(),
        }),
        identity,
        repository: backend_to_repository_connect(&repo.backend),
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            kind: "SnapshotPolicy".to_string(),
            name: policy_name.to_string(),
            namespace: namespace.to_string(),
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
    }
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

/// Resolve the recipe's source identity for the verify run (reuses the api kernel),
/// falling back to a sentinel for the quick tier (which does not restore).
fn verify_identity(
    config: &SnapshotPolicy,
    namespace: &str,
    repo: &ResolvedRepository,
) -> ResolvedIdentity {
    let first = config.spec.sources.first();
    let pvc_name = first.and_then(|s| s.pvc.as_ref().map(|p| p.name.clone()));
    let nfs_source_path = first.and_then(|s| s.nfs.as_ref().map(|n| n.path.clone()));
    let source_path_override = first.and_then(|s| s.source_path_override.clone());
    let inputs = kopiur_api::IdentityInputs {
        object_name: &config.name_any(),
        namespace,
        overrides: config.spec.identity.as_ref(),
        defaults: repo.identity_defaults.as_ref(),
        labels: config.metadata.labels.as_ref(),
        annotations: config.metadata.annotations.as_ref(),
        pvc_name: pvc_name.as_deref(),
        default_source_path: nfs_source_path.as_deref(),
        source_path_override: source_path_override.as_deref(),
    };
    match kopiur_api::resolve_identity(&inputs) {
        Ok(r) => ResolvedIdentity {
            username: r.username,
            hostname: r.hostname,
            source_path: r.source_path.unwrap_or_default(),
        },
        // A malformed identity is already rejected at admission; fall back to a
        // sentinel so the quick tier still runs (it doesn't use the path).
        Err(_) => ResolvedIdentity {
            username: "kopiur-verify".to_string(),
            hostname: namespace.to_string(),
            source_path: String::new(),
        },
    }
}

/// Whether the policy's resolved repository already carries **discovered** (adopted)
/// snapshots — the #168 verification escape hatch. A cheap labeled LIST scoped by the
/// repo-uid + `origin=discovered` labels the catalog scanner stamps on every
/// materialized foreign snapshot (`crate::catalog`); `true` as soon as one exists.
/// The [`DiscoveredProbeScope`] (from [`discovered_probe_scope`]) selects namespaced
/// vs cluster-wide so an adopted `ClusterRepository`'s scattered rows are seen. Thin
/// IO over the pure [`verification_unlocked`] gate.
async fn repo_has_discovered_snapshots(
    client: &kube::Client,
    scope: &DiscoveredProbeScope,
    repo_uid: &str,
) -> Result<bool> {
    let api: Api<Snapshot> = match scope {
        DiscoveredProbeScope::Namespace(ns) => Api::namespaced(client.clone(), ns),
        DiscoveredProbeScope::ClusterWide => Api::all(client.clone()),
    };
    let selector = format!(
        "{REPOSITORY_UID_LABEL}={repo_uid},{ORIGIN_LABEL}={}",
        Origin::Discovered.label_value()
    );
    let lp = ListParams::default().labels(&selector).limit(1);
    Ok(!api.list(&lp).await?.items.is_empty())
}

/// Whether any non-terminal verify Job is owned by this policy (single-flight gate).
async fn has_active_verify_job(job_api: &Api<Job>, policy_name: &str) -> Result<bool> {
    let selector =
        format!("{COMPONENT_LABEL}={VERIFY_COMPONENT},{VERIFY_INSTANCE_LABEL}={policy_name}");
    let jobs = job_api
        .list(&ListParams::default().labels(&selector))
        .await?;
    Ok(jobs.items.iter().any(|j| job_terminal_state(j).is_none()))
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn not_due_right_after_a_run() {
        let now = Utc::now();
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
                Some("America/Los_Angeles"),
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
                Some("America/Los_Angeles"),
                true
            )
            .is_some(),
            "the CronSpec's own timezone must win over the repo default"
        );
    }

    #[test]
    fn verify_job_name_is_deterministic_and_bounded() {
        let slot = DateTime::parse_from_rfc3339("2026-06-09T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let n = verify_job_name("postgres-data", VerifyTierKind::Quick, slot);
        assert!(n.len() <= 52);
        assert!(n.starts_with("postgres-data-vfy-q-"));
        assert_eq!(
            n,
            verify_job_name("postgres-data", VerifyTierKind::Quick, slot)
        );
        // Quick vs deep differ.
        assert_ne!(
            n,
            verify_job_name("postgres-data", VerifyTierKind::Deep, slot)
        );
        // Long names truncate+hash within budget.
        let long = "a-very-long-snapshot-policy-name-that-blows-the-dns-label-budget";
        assert!(verify_job_name(long, VerifyTierKind::Deep, slot).len() <= 52);
    }

    // --- work-spec mapping (tier → VerifyOp) ---

    fn sample_policy(verification: Verification) -> SnapshotPolicy {
        use kopiur_api::snapshot_policy::{PvcSource, Source};
        SnapshotPolicy::new(
            "pg",
            kopiur_api::SnapshotPolicySpec {
                repository: kopiur_api::common::RepositoryRef {
                    kind: Default::default(),
                    name: "r".into(),
                    namespace: None,
                },
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
            repo_namespace: Some("ns".into()),
            mover_defaults: None,
            identity_defaults: None,
            schedule_defaults: None,
            on_namespace_delete: Default::default(),
            credential_projection_allowed: false,
            owner_ref: Default::default(),
            mode: RepositoryMode::ReadWrite,
            deletion_protection: None,
            mass_deletion_ack: None,
        }
    }

    #[test]
    fn quick_work_spec_carries_quick_tier_and_success_expr() {
        let mut v = verification(Some("0 4 * * *"), None);
        v.success_expr = Some("stats.errors == 0".into());
        v.verify_files_percent = Some(10);
        let policy = sample_policy(v.clone());
        let repo = sample_repo();
        let ws = build_verify_work_spec(&policy, &repo, "ns", "pg", &v, VerifyTierKind::Quick);
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
        let ws = build_verify_work_spec(&policy, &repo, "ns", "pg", &v, VerifyTierKind::Quick);
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
        let ws = build_verify_work_spec(&policy, &repo, "ns", "pg", &v, VerifyTierKind::Deep);
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
        let ws = build_verify_work_spec(&policy, &repo, "ns", "pg", &v, VerifyTierKind::Deep);
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
}
