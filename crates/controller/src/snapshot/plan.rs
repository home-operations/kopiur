//! Pure decision functions for the `Snapshot` reconciler.
//!
//! Everything here is a pure function over CR/spec/status values — no `ctx`, no
//! kube IO, no `async`. These are the exhaustively-unit-tested decisions the
//! reconcile core in [`super`] wires to the cluster.

use std::collections::BTreeMap;

use kopiur_api::common::{NamespaceDeletePolicy, RepositoryKind, RepositoryRef};
use kopiur_api::snapshot::SnapshotPhase;
use kopiur_api::{DeletionPolicy, Origin, Snapshot, SnapshotPolicy};
use kopiur_mover::workspec::{MoverWorkSpec, ResolvedIdentity as MoverIdentity};
use kube::{Resource, ResourceExt};

use crate::consts::SKIP_SNAPSHOT_CLEANUP_ANNOTATION;
use crate::io;

/// The decision the deletion handler must execute. Derived purely from the
/// effective `DeletionPolicy` and the object's annotations — no IO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionPlan {
    /// Run `kopia snapshot delete <id>` (via a short Job) then remove the
    /// finalizer. On failure, stay in `phase: Deleting` and back off — the CR
    /// is NOT dropped (ADR §4.5).
    DeleteSnapshot,
    /// Remove the finalizer without contacting the repository (snapshot stays).
    /// Used by `Retain`.
    RetainSnapshot,
    /// Remove the finalizer without contacting the repository, record the
    /// snapshot orphaned, emit `SnapshotOrphaned`, bump the orphan metric. Used
    /// by `Orphan` and by the `skip-snapshot-cleanup` annotation escape hatch.
    OrphanSnapshot,
}

/// Decide what to do on deletion. **Exhaustive** over [`DeletionPolicy`] with no
/// catch-all: a new variant fails to compile until handled here (ADR §5.5).
///
/// The `skip-snapshot-cleanup` annotation is the repo-offline escape hatch and
/// **overrides everything** — even `Delete` — because its entire purpose is "the
/// bucket is gone, just let me remove the CR" (ADR §4.5).
pub fn plan_deletion(
    policy: DeletionPolicy,
    annotations: &BTreeMap<String, String>,
) -> DeletionPlan {
    if annotations.contains_key(SKIP_SNAPSHOT_CLEANUP_ANNOTATION) {
        return DeletionPlan::OrphanSnapshot;
    }
    match policy {
        DeletionPolicy::Delete => DeletionPlan::DeleteSnapshot,
        DeletionPolicy::Retain => DeletionPlan::RetainSnapshot,
        DeletionPolicy::Orphan => DeletionPlan::OrphanSnapshot,
    }
}

/// Reshape a per-`Snapshot` deletion plan for the **namespace-deletion** cascade
/// policy (ADR-0005 §5). This is the data-loss-prevention fix: a `kubectl delete ns`
/// must not silently destroy off-site backup history.
///
/// - When the owning namespace is NOT terminating, a single `kubectl delete snapshot`
///   honors the `Snapshot`'s own plan unchanged (`base_plan`).
/// - When the namespace IS terminating, the owning repository's
///   [`NamespaceDeletePolicy`] decides:
///   - `Orphan` (the fail-safe default) → force [`DeletionPlan::OrphanSnapshot`]:
///     remove the finalizer WITHOUT `kopia snapshot delete`, keeping history.
///   - `Delete` → opt-in cascade: fall through to the per-`Snapshot` `base_plan`.
///
/// Pure + exhaustive over [`NamespaceDeletePolicy`] (no `_ =>`), so a new variant
/// cannot compile until handled here (ADR §5.5). The fail-safe path is also taken
/// when the repository can't be resolved (repo already gone) — the caller passes
/// `Orphan` in that case.
pub fn namespace_delete_plan(
    policy: NamespaceDeletePolicy,
    ns_terminating: bool,
    base_plan: DeletionPlan,
) -> DeletionPlan {
    if !ns_terminating {
        return base_plan;
    }
    match policy {
        NamespaceDeletePolicy::Orphan => DeletionPlan::OrphanSnapshot,
        NamespaceDeletePolicy::Delete => base_plan,
    }
}

/// Where a `SnapshotDelete` Job may run. The Kubernetes `NamespaceLifecycle`
/// admission plugin rejects *creating* anything in a terminating namespace, so
/// the namespace-deletion cascade (ADR-0005 §5) can never run its delete Job in
/// the `Snapshot`'s own namespace — it must run where the repository's
/// credentials live, or fall back to orphaning (never wedge the namespace).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteJobPlacement {
    /// Create/poll the delete Job in this (non-terminating) namespace.
    RunIn(String),
    /// No surviving namespace can host the Job — orphan the snapshot instead
    /// (fail-safe: release the finalizer, keep the kopia snapshot, say why).
    OrphanFallback {
        /// Human-readable why + fix, surfaced in the `SnapshotOrphaned` event.
        reason: String,
    },
}

/// Decide where the `SnapshotDelete` Job runs. Pure, so the placement matrix is
/// unit-tested without a cluster:
///
/// - Namespace NOT terminating → the `Snapshot`'s own namespace (status quo).
/// - Terminating + namespaced `Repository` in a *different* namespace → the
///   repository's namespace (its credential Secret and any repo PVC live there).
/// - Terminating + `ClusterRepository` → the operator's namespace (where a
///   `ClusterRepository`'s canonical credential Secret lives, and where its
///   maintenance Jobs already run — ADR §3.7).
/// - Terminating + the repository (or operator) namespace IS the terminating
///   namespace, or the operator namespace is unknown → [`OrphanFallback`]:
///   nothing survivable can host the Job, and an uncreatable Job must not wedge
///   namespace deletion forever.
///
/// [`OrphanFallback`]: DeleteJobPlacement::OrphanFallback
pub fn delete_job_placement(
    ns_terminating: bool,
    snapshot_ns: &str,
    repo_namespace: Option<&str>,
    operator_namespace: Option<&str>,
) -> DeleteJobPlacement {
    if !ns_terminating {
        return DeleteJobPlacement::RunIn(snapshot_ns.to_string());
    }
    match repo_namespace {
        Some(rns) if rns != snapshot_ns => DeleteJobPlacement::RunIn(rns.to_string()),
        Some(_) => DeleteJobPlacement::OrphanFallback {
            reason: format!(
                "the Repository lives in `{snapshot_ns}`, the same namespace being deleted, so no \
                 surviving namespace can host the snapshot-delete Job; the kopia snapshot is \
                 orphaned instead — delete it manually with `kopia snapshot delete` if unwanted"
            ),
        },
        None => match operator_namespace {
            Some(op) if op != snapshot_ns => DeleteJobPlacement::RunIn(op.to_string()),
            Some(op) => DeleteJobPlacement::OrphanFallback {
                reason: format!(
                    "the operator namespace `{op}` is itself the namespace being deleted, so it \
                     cannot host the snapshot-delete Job; the kopia snapshot is orphaned instead"
                ),
            },
            None => DeleteJobPlacement::OrphanFallback {
                reason: "the operator namespace is unknown (KOPIUR_NAMESPACE is unset), so there \
                         is nowhere to run the ClusterRepository snapshot-delete Job during \
                         namespace deletion; set KOPIUR_NAMESPACE on the controller Deployment — \
                         the kopia snapshot is orphaned instead"
                    .to_string(),
            },
        },
    }
}

/// Normalize a recipe's `repository` ref for pinning into
/// `status.resolved.repository` (ADR §3.4, frozen at run time): a namespaced
/// `Repository` ref pins the namespace it actually resolved against (the
/// recipe's own namespace when unset) so the deletion path can re-resolve it
/// after the recipe is gone; a `ClusterRepository` ref pins none (the webhook
/// forbids one). Exhaustive over [`RepositoryKind`] (ADR §5.5).
pub fn pinned_repository_ref(r: &RepositoryRef, config_ns: &str) -> RepositoryRef {
    match r.kind {
        RepositoryKind::Repository => RepositoryRef {
            kind: RepositoryKind::Repository,
            name: r.name.clone(),
            namespace: Some(r.namespace.clone().unwrap_or_else(|| config_ns.to_string())),
        },
        RepositoryKind::ClusterRepository => RepositoryRef {
            kind: RepositoryKind::ClusterRepository,
            name: r.name.clone(),
            namespace: None,
        },
    }
}

pub use crate::naming::capped_name;

/// Build the `status.resolved` body frozen at run time (ADR §3.4): the
/// normalized repository ref ([`pinned_repository_ref`]) plus the concrete
/// source (PVC, when the recipe names one, and the kopia source path the work
/// spec actually snapshots). Pure — unit-tested without a cluster.
pub fn resolved_run_status(
    config: &SnapshotPolicy,
    namespace: &str,
    work_spec: &MoverWorkSpec,
) -> kopiur_api::snapshot::ResolvedSnapshot {
    let config_ns = config.namespace().unwrap_or_else(|| namespace.to_string());
    let pvc = config
        .spec
        .sources
        .first()
        .and_then(|s| s.pvc.as_ref())
        .map(|p| format!("{namespace}/{}", p.name));
    kopiur_api::snapshot::ResolvedSnapshot {
        repository: Some(pinned_repository_ref(&config.spec.repository, &config_ns)),
        sources: vec![kopiur_api::snapshot::ResolvedSource {
            pvc,
            source_path: Some(work_spec.identity.source_path.clone()),
        }],
    }
}

/// The mover identity pinned into `status.snapshot.identity` when the snapshot
/// succeeded — the identity the snapshot was actually recorded under. The
/// deletion path prefers it over re-deriving from a recipe that may since have
/// been edited or deleted (ADR §4.2: identity is resolved once, never
/// re-rendered).
pub(super) fn pinned_mover_identity(backup: &Snapshot) -> Option<MoverIdentity> {
    let id = &backup.status.as_ref()?.snapshot.as_ref()?.identity;
    Some(MoverIdentity {
        username: id.username.clone(),
        hostname: id.hostname.clone(),
        source_path: id
            .source_path
            .clone()
            .unwrap_or_else(|| "/data".to_string()),
    })
}

/// Map a `Snapshot` phase to its kstatus [`io::ReadyOutcome`] (ADR-0005 §2), so
/// `kubectl wait --for=condition=Ready` and Flux/Argo health work uniformly. Pure +
/// exhaustive: a new phase cannot compile until its Ready mapping is decided.
///
/// - `Succeeded`/`Discovered` → `Ready` (the snapshot exists / is catalogued).
/// - `Failed` → `Stalled` (terminal: won't progress without a spec change/retry).
/// - `Pending`/`Running`/`Deleting` → `Reconciling` (in flight).
pub fn snapshot_ready_outcome(phase: SnapshotPhase) -> io::ReadyOutcome {
    match phase {
        SnapshotPhase::Succeeded | SnapshotPhase::Discovered => io::ReadyOutcome::Ready,
        SnapshotPhase::Failed => io::ReadyOutcome::Stalled,
        SnapshotPhase::Pending | SnapshotPhase::Running | SnapshotPhase::Deleting => {
            io::ReadyOutcome::Reconciling
        }
    }
}

/// What the reconcile body may do for a produced `Snapshot` in `phase`, decided
/// BEFORE the mover Job is consulted. Pure + exhaustive: a new phase cannot
/// compile until its job-creation policy is chosen.
///
/// This is the one-shot discipline the `Restore` reconciler already applies: a
/// Snapshot that reached a terminal phase must NEVER mint another mover Job. The
/// owned Job self-reaps via `ttlSecondsAfterFinished`, and that deletion event
/// re-triggers this reconciler — keying "the work is done" on the Job's
/// *existence* (ephemeral) instead of the phase (durable) re-created the Job and
/// re-ran the whole backup after every TTL reap, forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunDecision {
    /// `Pending`/`Running`/no status yet: drive the mover Job (create or track).
    /// `Running` with a *missing* Job is the resume path — a mid-run Job can only
    /// vanish through outside deletion (the TTL applies after it finishes).
    Run,
    /// `Succeeded`: the kopia snapshot exists. Never touch the Job again; the
    /// only live surfaces are the staged-source reap and `spec.pin` drift.
    SucceededSteadyState,
    /// `Failed`: terminal until the spec changes (ADR: `Failed` → kstatus
    /// `Stalled`); a NEW Snapshot is how a retry happens.
    TerminalFailed,
    /// `Deleting`/`Discovered`: owned by earlier gates (the finalizer path and
    /// the Discovered pin). Reaching the run body in these phases is a watch
    /// desync — wait for a real change rather than acting on stale state.
    Wait,
}

/// Decide [`RunDecision`] from the observed phase (see the enum for semantics).
pub fn run_decision(phase: Option<SnapshotPhase>) -> RunDecision {
    match phase {
        None | Some(SnapshotPhase::Pending) | Some(SnapshotPhase::Running) => RunDecision::Run,
        Some(SnapshotPhase::Succeeded) => RunDecision::SucceededSteadyState,
        Some(SnapshotPhase::Failed) => RunDecision::TerminalFailed,
        Some(SnapshotPhase::Deleting) | Some(SnapshotPhase::Discovered) => RunDecision::Wait,
    }
}

/// Whether the preflight gate should run for a `Snapshot` in `phase`: only at first
/// launch (`None`/`Pending`). A `Running` snapshot whose mover Job vanished resumes
/// via the `run_decision == Run` path; re-evaluating preflight there could demote or
/// fail an in-flight backup on a since-flipped check, so it is excluded.
pub(super) fn should_run_preflight(phase: Option<SnapshotPhase>) -> bool {
    matches!(phase, None | Some(SnapshotPhase::Pending))
}

/// Whether the preflight deadline has passed: `preflight_since + timeout <= now`.
/// `timeout == None` ⇒ indefinite (never expires); `preflight_since == None` (the
/// failure just started this reconcile) ⇒ not expired. Pure / clock-injected.
pub(super) fn preflight_expired(
    preflight_since: Option<&str>,
    timeout: Option<std::time::Duration>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let (Some(t), Some(since)) = (
        timeout,
        preflight_since.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()),
    ) else {
        return false;
    };
    let elapsed = now - since.with_timezone(&chrono::Utc);
    elapsed >= chrono::Duration::from_std(t).unwrap_or(chrono::Duration::MAX)
}

/// Build the `(phase, observedGeneration, conditions)` status JSON for a `Snapshot`
/// reaching `phase`, deriving the kstatus Ready/Reconciling/Stalled conditions via
/// [`snapshot_ready_outcome`] + [`io::set_ready`]. Existing conditions (e.g.
/// `CredentialsAvailable`) are preserved by `set_ready`'s upsert.
pub(super) fn snapshot_ready_status(
    backup: &Snapshot,
    phase: SnapshotPhase,
    reason: &str,
    message: &str,
) -> serde_json::Value {
    snapshot_ready_status_over(backup, phase, reason, message, &existing_conditions(backup))
}

/// Like [`snapshot_ready_status`], but additionally upserts a domain condition
/// (e.g. `SourceStaged=False`) into the same write, so a terminal transition
/// carries both the specific condition and the derived kstatus set atomically.
pub(super) fn snapshot_ready_status_with_condition(
    backup: &Snapshot,
    phase: SnapshotPhase,
    reason: &str,
    message: &str,
    condition_type: &str,
    condition_status: bool,
) -> serde_json::Value {
    let seeded = io::upsert_condition(
        &existing_conditions(backup),
        condition_type,
        condition_status,
        reason,
        message,
        backup.meta().generation,
    );
    snapshot_ready_status_over(backup, phase, reason, message, &seeded)
}

fn existing_conditions(
    backup: &Snapshot,
) -> Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    backup
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default()
}

fn snapshot_ready_status_over(
    backup: &Snapshot,
    phase: SnapshotPhase,
    reason: &str,
    message: &str,
    existing: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> serde_json::Value {
    use kopiur_api::common::PhaseLabel;
    let generation = backup.meta().generation;
    let conditions = io::set_ready(
        existing,
        generation,
        snapshot_ready_outcome(phase),
        reason,
        message,
    );
    serde_json::json!({
        "phase": phase.label(),
        "observedGeneration": generation,
        "conditions": conditions,
    })
}

/// Compute the effective `DeletionPolicy` for a `Snapshot`, honoring the
/// origin-aware default (ADR §4.5): discovered backups are forced to `Retain`,
/// produced backups default to `Delete` when unset.
pub fn effective_deletion_policy(
    spec_policy: Option<DeletionPolicy>,
    origin: Origin,
) -> DeletionPolicy {
    match origin {
        // Discovered snapshots are never ours to delete — forced Retain.
        Origin::Discovered => DeletionPolicy::Retain,
        Origin::Scheduled | Origin::Manual => spec_policy.unwrap_or(DeletionPolicy::Delete),
    }
}

/// The kopia-side pin action a `Snapshot` reconcile must take (ADR-0005 §13(c)),
/// derived purely from `spec.pin` (desired) and `status.pinned` (observed). No IO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinAction {
    /// Apply the pin (`kopia snapshot pin --add`): desired `true`, not yet pinned.
    Pin,
    /// Remove the pin (`kopia snapshot pin --remove`): desired `false`, currently pinned.
    Unpin,
    /// Nothing to do — kopia's pin state already matches `spec.pin`.
    NoOp,
}

/// Decide the kopia-side pin action from the desired (`spec.pin`) and observed
/// (`status.pinned`) state. Pure + exhaustive so the decision is unit-tested and a
/// redundant `kopia snapshot pin` is never issued.
///
/// `observed == None` means we've never reconciled the pin: act iff `desired` is
/// `true` (apply it); a never-pinned snapshot with `desired == false` is already in
/// the right state, so `NoOp` (don't spawn an unpin for a pin that was never set).
pub fn pin_decision(desired: bool, observed: Option<bool>) -> PinAction {
    match (desired, observed) {
        (true, Some(true)) => PinAction::NoOp,
        (true, _) => PinAction::Pin,
        (false, Some(true)) => PinAction::Unpin,
        (false, _) => PinAction::NoOp,
    }
}

/// Resolve a `Snapshot`'s origin from its status (canonical) or its
/// `kopiur.home-operations.com/origin` label, defaulting to `Manual` when neither is present
/// (a bare `kubectl create`).
pub fn resolve_origin(b: &Snapshot) -> Origin {
    if let Some(o) = b.status.as_ref().and_then(|s| s.origin) {
        return o;
    }
    match b
        .labels()
        .get(crate::consts::ORIGIN_LABEL)
        .map(String::as_str)
    {
        Some("scheduled") => Origin::Scheduled,
        Some("discovered") => Origin::Discovered,
        _ => Origin::Manual,
    }
}
