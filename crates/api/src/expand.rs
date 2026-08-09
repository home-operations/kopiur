//! Selector expansion: turning one `SnapshotPolicy` source into N concrete
//! per-PVC backups (#346).
//!
//! # The model
//!
//! A `SnapshotPolicy` source is exactly one of `pvc`, `nfs`, or `pvcSelector`.
//! The first two name a single thing; the third matches many. Kopiur's whole
//! data model is built on **one `Snapshot` CR = one mover Job = one kopia
//! source path = one kopia manifest**, owned via a finalizer — retention,
//! restore, the catalog and the deletion policy all rest on that 1:1. So a
//! selector is expanded into N *ordinary* `Snapshot` CRs, one per matched PVC,
//! each of which is then indistinguishable from a hand-written single-PVC
//! backup.
//!
//! Expansion happens **once, at mint time**, in whichever component creates the
//! CR — a `SnapshotSchedule` fire or `kubectl kopiur snapshot now`. A `Snapshot`
//! never expands itself: a CR minting sibling CRs would re-enter the same
//! reconciler for each child and break the one-shot `run_decision` discipline,
//! and a `SnapshotPolicy` that minted invocations would collapse the
//! recipe/invocation/schedule split the project is built around.
//!
//! # What was here before
//!
//! Nothing. `pvcSelector` was schema-valid, admission-accepted, documented on
//! six pages and shipped as `deploy/examples/04-multi-pvc-selector.yaml` — and
//! no expansion code existed anywhere, so `build_backup_run` hit its
//! `_ =>` arm and returned `invariant violated … This is likely a bug in
//! kopiur`. That is #346.

use std::collections::BTreeMap;

use crate::error::ValidationError;
use crate::snapshot::{PvcTargetRef, SnapshotSourceGroup, SnapshotSourceRef, SnapshotSourceTarget};
use crate::snapshot_policy::SnapshotPolicy;
use crate::snapshot_policy::{self, Source, SourcePathStrategy};
use kube::ResourceExt;

/// Max name length for a `Snapshot` CR produced by expansion.
///
/// **63, not 253.** The CR's name becomes the mover `Job`'s name, and
/// `io::cleanup_staged_source` finds that Job's pods by the
/// `batch.kubernetes.io/job-name` **label value**, which Kubernetes caps at 63
/// bytes. A longer name silently breaks the pvc-protection release step and
/// wedges staged-PVC teardown. Fan-out makes long names routine, so this is
/// enforced here rather than discovered in production.
const MAX_CHILD_NAME: usize = 63;

/// The marker segment that makes a fanned-out name unambiguous.
const FANOUT_MARKER: &str = "-pvc-";

// --- the effective source for one run --------------------------------------

/// The single source one `Snapshot` actually backs up, after resolving
/// `spec.source` (a fanned-out child) against the policy's `sources[]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveSource {
    /// Index into `policy.spec.sources` these knobs came from.
    pub index: usize,
    /// The concrete PVC, when this is a PVC-shaped source.
    pub pvc: Option<PvcTargetRef>,
    /// The NFS export path, when this is an NFS source.
    pub nfs_path: Option<String>,
    /// `sourcePathOverride` from the governing source.
    pub source_path_override: Option<String>,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

impl EffectiveSource {
    /// The kopia source path (and mount path) for this run.
    ///
    /// `sourcePathOverride` wins. Otherwise a PVC yields `/pvc/<name>` or
    /// `/pvc/<namespace>/<name>` depending on the governing source's
    /// [`SourcePathStrategy`], and an NFS source yields its export path.
    pub fn kopia_source_path(&self, strategy: SourcePathStrategy) -> Option<String> {
        if let Some(o) = &self.source_path_override {
            return Some(o.clone());
        }
        if let Some(p) = &self.pvc {
            return Some(match strategy {
                SourcePathStrategy::PvcName => format!("/pvc/{}", p.name),
                SourcePathStrategy::PvcNamespacedName => {
                    format!("/pvc/{}/{}", p.namespace, p.name)
                }
            });
        }
        self.nfs_path.clone()
    }
}

/// Resolve which source this `Snapshot` covers.
///
/// * `pin: None` — the ordinary single-source case: `sources[0]` governs, and
///   its own `pvc`/`nfs` is the target. Byte-for-byte the old behavior.
/// * `pin: Some(_)` — a fanned-out child: `sourceIndex` selects the governing
///   source's knobs, and the pinned `target` is the concrete PVC.
///
/// An out-of-range `sourceIndex` (the policy shrank mid-run) is a named,
/// terminal error. It is emphatically NOT a fallback to `sources[0]`: silently
/// backing up a different volume than the one the CR names is the worst
/// possible failure mode for a backup operator.
pub fn effective_source(
    policy: &SnapshotPolicy,
    pin: Option<&SnapshotSourceRef>,
) -> Result<EffectiveSource, ValidationError> {
    let sources = &policy.spec.sources;
    let index = pin.map(|p| p.source_index as usize).unwrap_or(0);
    let Some(source) = sources.get(index) else {
        return Err(ValidationError::InvalidFieldValue {
            field: "spec.source.sourceIndex".to_string(),
            reason: format!(
                "`spec.source.sourceIndex` is {index} but SnapshotPolicy `{}` now has {} source(s); \
             the recipe was edited after this Snapshot was created. Delete this Snapshot and let \
             the schedule re-fire, or recreate it against the current recipe.",
                policy.name_any(),
                sources.len()
            ),
        });
    };
    let read_only = snapshot_policy::source_read_only(source);
    let common = |pvc: Option<PvcTargetRef>| EffectiveSource {
        index,
        pvc,
        nfs_path: source.nfs.as_ref().map(|n| n.path.clone()),
        source_path_override: source.source_path_override.clone(),
        read_only,
    };
    match pin.map(|p| &p.target) {
        // A fanned-out child names its own PVC; the policy source it came from
        // is a selector and has no `pvc` of its own.
        Some(SnapshotSourceTarget::Pvc(t)) => Ok(common(Some(t.clone()))),
        None => Ok(common(source.pvc.as_ref().map(|p| PvcTargetRef {
            // A non-selector `pvc:` source is always same-namespace.
            namespace: policy.namespace().unwrap_or_default(),
            name: p.name.clone(),
        }))),
    }
}

/// The `sourcePathStrategy` governing a source.
///
/// Deliberately only consulted for **selector-expanded** sources. A plain
/// `pvc:` source keeps `/pvc/<name>` unconditionally: changing the path of an
/// existing single-PVC policy would re-identify its kopia source and orphan
/// every manifest it has ever taken.
pub fn strategy_for(source: &Source) -> SourcePathStrategy {
    if source.pvc_selector.is_some() {
        source
            .source_path_strategy
            .unwrap_or(SourcePathStrategy::PvcName)
    } else {
        SourcePathStrategy::PvcName
    }
}

/// Render a `LabelSelector` as the API server's selector string.
///
/// Lives here so the controller and the CLI build byte-identical queries: a
/// divergence would make `kubectl kopiur snapshot now` and the schedule expand
/// to DIFFERENT PVC sets for the same recipe, which is the kind of discrepancy
/// nobody notices until a restore is missing a volume.
pub fn label_selector_string(
    sel: &k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector,
) -> String {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement;
    let mut terms: Vec<String> = Vec::new();
    if let Some(labels) = &sel.match_labels {
        for (k, v) in labels {
            terms.push(format!("{k}={v}"));
        }
    }
    if let Some(exprs) = &sel.match_expressions {
        for LabelSelectorRequirement {
            key,
            operator,
            values,
        } in exprs
        {
            let vals = values.clone().unwrap_or_default().join(",");
            match operator.as_str() {
                "In" => terms.push(format!("{key} in ({vals})")),
                "NotIn" => terms.push(format!("{key} notin ({vals})")),
                "Exists" => terms.push(key.clone()),
                "DoesNotExist" => terms.push(format!("!{key}")),
                // Unknown operator: skip (the webhook/schema constrain the set).
                _ => {}
            }
        }
    }
    terms.join(",")
}

// --- naming -----------------------------------------------------------------

/// The deterministic name of the fanned-out `Snapshot` for one PVC.
///
/// `<base>-pvc-<slug>-<h8>`, capped at [`MAX_CHILD_NAME`].
///
/// * `<slug>` is human-legible (`<name>`, or `<namespace>-<name>` when the PVC
///   is outside the policy's namespace) and may be clipped.
/// * `<h8>` is 8 hex of FNV-1a over the exact string `"<namespace>/<name>"` and
///   is **never** clipped — it is the injectivity guarantee.
///
/// Collision-free against the three existing schemes (`<schedule>-<slot>`,
/// `<schedule>-<policy>-<slot>`, `<policy>-manual-<slot>`): each ends in a
/// dash-free 14-digit slot stamp, while a fanned name's tail after `-pvc-`
/// always contains a `-`.
///
/// This does NOT delegate to `io::staging::staged_child_name`, whose
/// `MAX_NAME_LEN - tag.len() - suffix.len() - 2` is unchecked `usize`
/// subtraction: its callers pass 4-char suffixes (`snap`, `src`), and a PVC
/// name up to 253 chars would underflow it.
///
/// ```
/// # use kopiur_api::expand::fanout_child_name;
/// let n = fanout_child_name("nightly-20260805020000", "db", "db", "pgdata");
/// assert!(n.starts_with("nightly-20260805020000-pvc-pgdata-"));
/// assert!(n.len() <= 63);
/// ```
pub fn fanout_child_name(base: &str, policy_ns: &str, pvc_ns: &str, pvc_name: &str) -> String {
    fanout_child_name_for(base, policy_ns, Some((pvc_ns, pvc_name)), None)
}

/// The marker segment naming the target repository in a multi-repo child name.
const REPO_MARKER: &str = "-repo-";

/// 8 hex chars of FNV-1a over `input` — the never-clipped injectivity tag
/// shared by every fan-out naming scheme in this module. One definition so
/// the hash function can never drift between the schemes.
fn fnv8(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in input.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{:08x}", (hash & 0xffff_ffff) as u32)
}

/// The deterministic child `Snapshot` name for one (source-member, repository)
/// cell of a fan-out — the general form behind [`fanout_child_name`].
///
/// * `member`: `Some((pvc_ns, pvc_name))` for a selector-expanded member,
///   `None` for a policy whose single source needs no per-PVC expansion.
/// * `repo`: `Some(ref)` for a multi-repository fan-out child, `None` for the
///   classic single-repo shapes.
///
/// The four combinations produce:
///
/// | member | repo | name |
/// |--------|------|------|
/// | `None` | `None` | `<base>` — the legacy non-fanned name, byte-identical |
/// | `Some` | `None` | `<base>-pvc-<pslug>-<h8>` — byte-identical to the legacy [`fanout_child_name`] |
/// | `None` | `Some` | `<base>-repo-<rslug>-<h8>` |
/// | `Some` | `Some` | `<base>-pvc-<pslug>-repo-<rslug>-<h8>` |
///
/// `<h8>` is FNV-1a over the newline-framed FULL unclipped tuple — `base`,
/// then (when present) `"<pvc_ns>/<pvc_name>"`, then (when present) the
/// normalized [`repo_key`](crate::common::repo_key) — so clipping any legible
/// segment never merges two distinct cells, and a policy slug that happens to
/// contain `-repo-` (pslug `x-repo-y` vs pslug `x` + rslug `y`) still yields
/// distinct names because the hash inputs differ. Markers and the tag are
/// NEVER clipped; when over budget the clip order is base → pslug → rslug.
/// Always ≤ [`MAX_CHILD_NAME`] (63).
///
/// The legacy hash input for `repo: None` is exactly the pre-multi-repo
/// `"{base}\n{pvc_ns}/{pvc_name}"` — pinned by golden tests.
pub fn fanout_child_name_for(
    base: &str,
    policy_ns: &str,
    member: Option<(&str, &str)>,
    repo: Option<&crate::common::RepositoryRef>,
) -> String {
    // (normalized repo_key for the hash, legible slug for the name).
    let repo = repo.map(|r| (crate::common::repo_key(r, policy_ns), r.name.as_str()));
    match (member, repo) {
        // Legacy single child: the name IS the base (already ≤63 for every
        // caller-produced base; clipped defensively, identical when in budget).
        (None, None) => clip(base, MAX_CHILD_NAME).trim_matches('-').to_string(),
        (Some((pvc_ns, pvc_name)), repo) => {
            member_child_name(base, policy_ns, pvc_ns, pvc_name, repo.as_ref())
        }
        (None, Some((rkey, repo_name))) => {
            // <base>-repo-<rslug>-<h8>
            let tag = fnv8(&format!("{base}\n{rkey}"));
            let rslug = sanitize_dns1123(repo_name);
            let fixed = REPO_MARKER.len() + 1 + tag.len();
            let mut base_keep = base.len();
            let mut rslug_keep = rslug.len();
            if fixed + base_keep + rslug_keep > MAX_CHILD_NAME {
                let room = MAX_CHILD_NAME.saturating_sub(fixed);
                // Clip base first (every sibling shares it), rslug second.
                rslug_keep = rslug_keep.min(room / 3);
                base_keep = base_keep.min(room.saturating_sub(rslug_keep));
            }
            format!(
                "{}{REPO_MARKER}{}-{tag}",
                clip(base, base_keep),
                clip(&rslug, rslug_keep)
            )
            .trim_matches('-')
            .to_string()
        }
    }
}

/// The `member: Some(..)` arm of [`fanout_child_name_for`]: the legacy
/// `-pvc-` form (`rkey: None`, byte-identical to the pre-multi-repo
/// [`fanout_child_name`]) and the combined `-pvc-…-repo-…` form.
fn member_child_name(
    base: &str,
    policy_ns: &str,
    pvc_ns: &str,
    pvc_name: &str,
    repo: Option<&(String, &str)>,
) -> String {
    // The tag hashes the BASE as well as the PVC (and, for a multi-repo child,
    // the repo key). That is not belt-and-braces: `base` is
    // `<schedule>-<YYYYMMDDHHMMSS>` and it is the part that gets CLIPPED when
    // the name is too long, so a tag over the PVC alone leaves two different
    // slots of the same schedule+PVC with byte-identical names. The schedule
    // server-side-applies with force and only skips *terminating* twins, so
    // the second fire would re-apply onto the already-`Succeeded` first
    // Snapshot, `run_decision` would return `SucceededSteadyState`, and no
    // mover Job would ever launch — a whole backup slot vanishing with no
    // error anywhere.
    let tag = match repo {
        None => fnv8(&format!("{base}\n{pvc_ns}/{pvc_name}")),
        Some((rkey, _)) => fnv8(&format!("{base}\n{pvc_ns}/{pvc_name}\n{rkey}")),
    };

    let slug_full = if pvc_ns == policy_ns {
        pvc_name.to_string()
    } else {
        format!("{pvc_ns}-{pvc_name}")
    };
    let slug = sanitize_dns1123(&slug_full);

    match repo {
        None => {
            // Budget: base + "-pvc-" + slug + "-" + tag. Clip `base` first (it
            // is the most redundant part — every sibling shares it), then the
            // slug. Never the tag or the marker.
            let fixed = FANOUT_MARKER.len() + 1 + tag.len();
            let mut base_keep = base.len();
            let mut slug_keep = slug.len();
            if fixed + base_keep + slug_keep > MAX_CHILD_NAME {
                let room = MAX_CHILD_NAME.saturating_sub(fixed);
                // Give the slug up to a third of the room, the base the rest.
                slug_keep = slug_keep.min(room / 3);
                base_keep = base_keep.min(room.saturating_sub(slug_keep));
            }
            let name = format!(
                "{}{FANOUT_MARKER}{}-{tag}",
                clip(base, base_keep),
                clip(&slug, slug_keep)
            );
            // A clip can leave a trailing '-', which is not a legal DNS-1123 name.
            name.trim_matches('-').to_string()
        }
        Some((_, repo_name)) => {
            // Combined form: base + "-pvc-" + pslug + "-repo-" + rslug + "-" +
            // tag. Overhead 5+6+1+8 = 20, room 43. Clip base → pslug → rslug.
            let rslug = sanitize_dns1123(repo_name);
            let fixed = FANOUT_MARKER.len() + REPO_MARKER.len() + 1 + tag.len();
            let mut base_keep = base.len();
            let mut pslug_keep = slug.len();
            let mut rslug_keep = rslug.len();
            if fixed + base_keep + pslug_keep + rslug_keep > MAX_CHILD_NAME {
                let room = MAX_CHILD_NAME.saturating_sub(fixed);
                rslug_keep = rslug_keep.min(room / 3);
                pslug_keep = pslug_keep.min(room.saturating_sub(rslug_keep) / 2);
                base_keep = base_keep.min(room.saturating_sub(rslug_keep + pslug_keep));
            }
            format!(
                "{}{FANOUT_MARKER}{}{REPO_MARKER}{}-{tag}",
                clip(base, base_keep),
                clip(&slug, pslug_keep),
                clip(&rslug, rslug_keep)
            )
            .trim_matches('-')
            .to_string()
        }
    }
}

/// The kopia-cache PVC name for one (policy, repository) cell.
///
/// A kopia client cache is REPOSITORY-SPECIFIC state (indexes, metadata, owned
/// blobs of ONE repository), and the mover mounts it at a fixed
/// `KOPIA_CACHE_DIRECTORY` — so a multi-repo policy's children must not share
/// one PVC or they poison each other's cache. Hence:
///
/// * `repo: None` — the classic single-repo cache: `kopiur-cache-<policy>`,
///   byte-identical to the pre-multi-repo name (including its historical lack
///   of a length cap — an existing PVC must keep matching by name).
/// * `repo: Some(_)` — a pinned child of a multi-repo policy:
///   `kopiur-cache-<policy>-<rslug>-<h6>`, where `<h6>` is 6 hex of FNV-1a
///   over the normalized [`repo_key`](crate::common::repo_key) (never
///   clipped — the injectivity guarantee, same family as
///   [`fanout_child_name_for`]) and the whole name is capped at 63 (RFC 1123
///   label, the same Job-label bound child names honor). Clip order:
///   policy slug first, then rslug; marker dashes and the tag never.
///
/// Ownership stays the policy in both shapes (the caller's concern).
pub fn cache_pvc_name(
    policy_name: &str,
    policy_ns: &str,
    repo: Option<&crate::common::RepositoryRef>,
) -> String {
    const PREFIX: &str = "kopiur-cache-";
    let Some(repo) = repo else {
        return format!("{PREFIX}{policy_name}");
    };
    let rkey = crate::common::repo_key(repo, policy_ns);
    // h6: the fnv8 family hash, truncated to 6 hex — enough to disambiguate a
    // policy's ≤8 repositories while keeping the legible slugs roomy.
    let tag: String = fnv8(&rkey).chars().take(6).collect();
    let pslug = sanitize_dns1123(policy_name);
    let rslug = sanitize_dns1123(&repo.name);
    let fixed = PREFIX.len() + 2 + tag.len(); // two joining dashes + tag
    let mut pslug_keep = pslug.len();
    let mut rslug_keep = rslug.len();
    if fixed + pslug_keep + rslug_keep > MAX_CHILD_NAME {
        let room = MAX_CHILD_NAME.saturating_sub(fixed);
        rslug_keep = rslug_keep.min(room / 3);
        pslug_keep = pslug_keep.min(room.saturating_sub(rslug_keep));
    }
    // Trim any clip-produced trailing '-' per segment (a doubled dash is not a
    // legal-name problem, but keeping segments clean avoids `--` cosmetics; the
    // never-clipped tag carries injectivity regardless).
    format!(
        "{PREFIX}{}-{}-{tag}",
        clip(&pslug, pslug_keep).trim_end_matches('-'),
        clip(&rslug, rslug_keep).trim_end_matches('-')
    )
    .trim_matches('-')
    .to_string()
}

/// Truncate on a char boundary.
fn clip(s: &str, keep: usize) -> &str {
    if keep >= s.len() {
        return s;
    }
    let mut end = keep;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Lowercase, replacing anything outside `[a-z0-9-]` with `-`.
fn sanitize_dns1123(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

// --- expansion ---------------------------------------------------------------

/// One member of an expanded selector: the child's name and the `spec.source`
/// to stamp on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedMember {
    /// Deterministic child `Snapshot` name.
    pub name: String,
    /// The pin recording which PVC this child covers.
    pub source: SnapshotSourceRef,
}

/// The shared `VolumeGroupSnapshot` name for one expansion in one namespace.
///
/// Derived from the per-invocation base name, NOT from any one member: every
/// member must compute the identical name without talking to its siblings,
/// which is what makes their racing server-side-applies converge on one object
/// instead of N.
///
/// Bounded to 63 chars for the same reason child names are — see
/// [`fanout_child_name`].
/// `repo_key` adds the repository dimension for multi-repo fan-out — one
/// VolumeGroupSnapshot per (repository, slot), because each repo's members are
/// an independent capture wave (independent-captures semantics: the N groups
/// are N separate point-in-time CSI snapshots, at N× the CSI quota/load).
/// Existing callers pass `None` and get the byte-identical legacy name.
pub fn group_name(
    base: &str,
    namespace: &str,
    source_index: usize,
    repo_key: Option<&str>,
) -> String {
    let suffix = "-grp";
    // The SOURCE INDEX is part of the key, not just the namespace. Two selector
    // sources in one policy have DIFFERENT label selectors, and
    // `resolve_group_stage` builds the VolumeGroupSnapshot's `source.selector`
    // from `sources[sourceIndex]`. Sharing a name across them would have the two
    // members force-SSA conflicting selectors onto one object, so the loser's
    // PVC is never captured and it fails with `GroupMemberMissing`.
    let tag = match repo_key {
        None => fnv8(&format!("{namespace}#{source_index}")),
        Some(k) => fnv8(&format!("{namespace}#{source_index}#{k}")),
    };
    let room = MAX_CHILD_NAME - suffix.len() - tag.len() - 1;
    format!("{}-{tag}{suffix}", clip(base, room))
        .trim_matches('-')
        .to_string()
}

/// One `Snapshot` to mint for a slot/invocation: its deterministic name, the
/// source pin (for a `pvcSelector` member) and the repository pin (for a
/// multi-repo fan-out child, NORMALIZED via
/// [`normalized_repository_ref`](crate::common::normalized_repository_ref)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintCell {
    /// Deterministic child `Snapshot` name.
    pub name: String,
    /// The `spec.source` pin recording which PVC this child covers.
    pub source: Option<SnapshotSourceRef>,
    /// The `spec.repository` pin recording which repository this child targets
    /// (multi-repo fan-out only; `None` keeps the legacy unpinned wire).
    pub repository: Option<crate::common::RepositoryRef>,
}

/// **Pure.** Cross one policy's expanded source members with its repository
/// dimension (#368) into the exact set of `Snapshot`s to mint for one slot or
/// one `snapshot now` invocation. Lives here so the `SnapshotSchedule`
/// reconciler and `kubectl kopiur snapshot now` mint byte-identical sets — a
/// divergence would give the same recipe different children depending on who
/// fired it.
///
/// * Single-repo policy: byte-identical to the pre-multi-repo behavior — the
///   bare `base_name` child (no pins) for a selector-less recipe, or one
///   unpinned per-member child.
/// * Multi-repo policy: one child per (member × repository), named via
///   [`fanout_child_name_for`] with the repo dimension, `spec.repository`
///   stamped NORMALIZED; a grouped member's shared `VolumeGroupSnapshot` name
///   is re-derived PER REPOSITORY ([`group_name`] with the repo key) — each
///   repo's members are an independent capture wave, so N repos = N groups.
///
/// `members: Some(vec![])` (a selector that matched nothing) yields no cells —
/// the caller warns, as before.
pub fn mint_cells(
    policy: &SnapshotPolicy,
    base_name: &str,
    members: Option<Vec<ExpandedMember>>,
) -> Vec<MintCell> {
    use crate::common::{normalized_repository_ref, repo_key};
    let policy_ns = policy.namespace().unwrap_or_default();
    let repos: Option<Vec<&crate::common::RepositoryRef>> =
        crate::snapshot_policy::is_multi_repo(&policy.spec)
            .then(|| policy.spec.repositories.iter().collect());
    match (members, repos) {
        // Single-repo, no selector: the legacy bare child.
        (None, None) => vec![MintCell {
            name: base_name.to_string(),
            source: None,
            repository: None,
        }],
        // Single-repo selector fan-out: unpinned members, byte-identical.
        (Some(members), None) => members
            .into_iter()
            .map(|m| MintCell {
                name: m.name,
                source: Some(m.source),
                repository: None,
            })
            .collect(),
        // Multi-repo, no selector: one child per repository.
        (None, Some(repos)) => repos
            .into_iter()
            .map(|r| MintCell {
                name: fanout_child_name_for(base_name, &policy_ns, None, Some(r)),
                source: None,
                repository: Some(normalized_repository_ref(r, &policy_ns)),
            })
            .collect(),
        // Multi-repo selector fan-out: members × repositories.
        (Some(members), Some(repos)) => members
            .iter()
            .flat_map(|m| {
                let policy_ns = policy_ns.clone();
                let target = match &m.source.target {
                    SnapshotSourceTarget::Pvc(t) => t.clone(),
                };
                repos.iter().map(move |r| {
                    let rkey = repo_key(r, &policy_ns);
                    let mut source = m.source.clone();
                    if let Some(group) = source.group.as_mut() {
                        group.volume_group_snapshot_name = group_name(
                            base_name,
                            &group.namespace,
                            source.source_index as usize,
                            Some(&rkey),
                        );
                    }
                    MintCell {
                        name: fanout_child_name_for(
                            base_name,
                            &policy_ns,
                            Some((&target.namespace, &target.name)),
                            Some(r),
                        ),
                        source: Some(source),
                        repository: Some(normalized_repository_ref(r, &policy_ns)),
                    }
                })
            })
            .collect(),
    }
}

/// **Pure.** Expand one policy's sources against an already-matched PVC set.
///
/// `matched` is `(namespace, name)` per source index — the caller does the
/// cluster IO ([`match_pvcs`]), this decides the names and pins.
///
/// Returns `Ok(None)` when the policy has no selector source at all, meaning
/// "mint exactly one child with no `spec.source`", i.e. today's behavior. That
/// distinction matters: an empty `Vec` would mean "a selector matched nothing",
/// which is a different and much louder situation.
///
/// # The collision guard
///
/// `sourcePathStrategy` defaults to `PvcName` → `/pvc/<name>`. A selector with
/// a cross-namespace `namespaceSelector` matching a PVC called `data` in two
/// namespaces therefore yields **two kopia sources at the identical
/// `user@host:/pvc/data`**, silently merging two volumes' histories into one
/// stream — under a single `KOPIA_KEEP_MAX` retention pin, so they also prune
/// each other. `detect_identity_collision` cannot catch this: it compares
/// across policies and skips self. So it is caught here, before anything is
/// created.
pub fn expand_sources(
    policy: &SnapshotPolicy,
    base_name: &str,
    matched: &BTreeMap<usize, Vec<PvcTargetRef>>,
) -> Result<Option<Vec<ExpandedMember>>, ValidationError> {
    let policy_ns = policy.namespace().unwrap_or_default();
    if !policy.spec.sources.iter().any(|s| s.pvc_selector.is_some()) {
        return Ok(None);
    }
    let mut members: Vec<ExpandedMember> = Vec::new();
    // path -> (target, source index) that first produced it.
    let mut paths: BTreeMap<String, (PvcTargetRef, usize)> = BTreeMap::new();
    let grouped =
        policy.spec.group_by == Some(crate::snapshot_policy::GroupBy::VolumeGroupSnapshot);
    // How many members land in each namespace, computed up front: a
    // VolumeGroupSnapshot is namespaced and its `source.selector` is
    // namespace-local, so a selector spanning namespaces yields ONE GROUP PER
    // NAMESPACE — the consistency guarantee is per-namespace, not global.
    // Keyed by SOURCE too, not just namespace: one VolumeGroupSnapshot is built
    // from ONE source's label selector, so two selector sources in the same
    // namespace are two separate captures, and counting them together would
    // wrongly promote a pair of one-PVC sources into a "group".
    let mut per_group: BTreeMap<(&str, usize), usize> = BTreeMap::new();
    if grouped {
        for (index, source) in policy.spec.sources.iter().enumerate() {
            if source.pvc_selector.is_none() {
                continue;
            }
            for t in matched.get(&index).into_iter().flatten() {
                *per_group.entry((t.namespace.as_str(), index)).or_default() += 1;
            }
        }
    }

    for (index, source) in policy.spec.sources.iter().enumerate() {
        if source.pvc_selector.is_none() {
            continue;
        }
        let strategy = strategy_for(source);
        for target in matched.get(&index).into_iter().flatten() {
            // Collision check against every path this expansion has produced.
            let eff = EffectiveSource {
                index,
                pvc: Some(target.clone()),
                nfs_path: None,
                source_path_override: source.source_path_override.clone(),
                read_only: snapshot_policy::source_read_only(source),
            };
            let path = eff
                .kopia_source_path(strategy)
                .unwrap_or_else(|| "/data".to_string());
            // ANY repeated path is refused, not just one produced by two
            // DIFFERENT PVCs. Two selector sources that both match the same PVC
            // land on one path AND one child name, so the second would
            // force-server-side-apply over the first and one backup would
            // vanish with no error — the same silent-overwrite class as a
            // clipped slot stamp.
            if let Some((prev, prev_index)) = paths.insert(path.clone(), (target.clone(), index)) {
                let same_pvc = prev.namespace == target.namespace && prev.name == target.name;
                // The same PVC listed twice by the SAME source is a listing
                // artifact (`match_pvcs` already dedupes; this keeps
                // `expand_sources` idempotent for any caller). Skip it rather
                // than reporting a configuration error that isn't one.
                if same_pvc && prev_index == index {
                    continue;
                }
                let reason = if same_pvc {
                    format!(
                        "SnapshotPolicy `{}` has two `pvcSelector` sources that both match \
                         `{}/{}`, so it would try to back that one volume up twice at the same \
                         kopia source path `{path}`. Narrow the selectors so each PVC is matched \
                         by exactly one source.",
                        policy.name_any(),
                        target.namespace,
                        target.name,
                    )
                } else {
                    format!(
                        "SnapshotPolicy `{}`'s pvcSelector matches both `{}/{}` and `{}/{}`, \
                         which resolve to the SAME kopia source path `{path}` under \
                         `sourcePathStrategy: PvcName`. Their backups would merge into one \
                         snapshot history and prune each other. Set `sourcePathStrategy: \
                         PvcNamespacedName` on that source.",
                        policy.name_any(),
                        prev.namespace,
                        prev.name,
                        target.namespace,
                        target.name,
                    )
                };
                return Err(ValidationError::InvalidFieldValue {
                    field: "spec.sources[].pvcSelector".to_string(),
                    reason,
                });
            }
            members.push(ExpandedMember {
                name: fanout_child_name(base_name, &policy_ns, &target.namespace, &target.name),
                source: SnapshotSourceRef {
                    source_index: index as u32,
                    target: SnapshotSourceTarget::Pvc(target.clone()),
                    // A one-member "group" buys nothing and costs a
                    // VolumeGroupSnapshotClass requirement (and a Beta API
                    // group many clusters do not serve), so it degrades to the
                    // ordinary per-PVC VolumeSnapshot path.
                    group: (grouped
                        && per_group
                            .get(&(target.namespace.as_str(), index))
                            .copied()
                            .unwrap_or(0)
                            > 1)
                    .then(|| SnapshotSourceGroup {
                        namespace: target.namespace.clone(),
                        // Repo dimension: none yet — the multi-repo fan-out
                        // consumers of `group_name(.., Some(repo_key))` land in
                        // M8/M10; passing None keeps names byte-identical.
                        volume_group_snapshot_name: group_name(
                            base_name,
                            &target.namespace,
                            index,
                            None,
                        ),
                    }),
                },
            });
        }
    }
    Ok(Some(members))
}

#[cfg(test)]
mod tests;
