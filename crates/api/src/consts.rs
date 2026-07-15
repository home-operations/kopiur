//! Well-known wire-contract strings: the finalizer, labels, annotations, and
//! condition types that form kopiur's public Kubernetes surface (ADR §4.5,
//! ADR-0005 §2/§14(c)).
//!
//! These live in `kopiur-api` — not the controller — because they are part of
//! the API contract itself: external tooling (the `kubectl kopiur` plugin,
//! GitOps health checks, user automation) must agree on them byte-for-byte
//! with the operator. Controller-internal reasons/actions/deadlines stay in
//! `kopiur-controller`'s own `consts` module.

/// The finalizer every `Snapshot` carries so the operator can run snapshot
/// cleanup before the CR is removed (ADR §4.5 / SKILL "Snapshot lifecycle =
/// CR lifecycle").
pub const SNAPSHOT_CLEANUP_FINALIZER: &str = "kopiur.home-operations.com/snapshot-cleanup";

/// Repo-offline escape hatch: when present, the finalizer is removed *without*
/// contacting the repository, the snapshot is recorded orphaned, and a
/// `SnapshotOrphaned` event is emitted (ADR §4.5).
pub const SKIP_SNAPSHOT_CLEANUP_ANNOTATION: &str =
    "kopiur.home-operations.com/skip-snapshot-cleanup";

/// Label mirroring a `Snapshot`'s origin (`scheduled`/`manual`/`discovered`).
pub const ORIGIN_LABEL: &str = "kopiur.home-operations.com/origin";
/// Label keying a discovered `Snapshot` to its kopia snapshot id (dedup, §2.1).
pub const SNAPSHOT_ID_LABEL: &str = "kopiur.home-operations.com/snapshot-id";
/// Label keying a discovered `Snapshot` to the owning Repository UID (dedup).
pub const REPOSITORY_UID_LABEL: &str = "kopiur.home-operations.com/repository-uid";
/// Label naming the `SnapshotPolicy` a `Snapshot` was produced from.
pub const CONFIG_LABEL: &str = "kopiur.home-operations.com/config";

/// Label naming the `SnapshotSchedule` that fired a scheduled `Snapshot`
/// (selector for a schedule's own children, distinct from [`CONFIG_LABEL`]
/// under `policySelector` fan-out).
pub const SCHEDULE_LABEL: &str = "kopiur.home-operations.com/schedule";

/// Label naming the operation a mover `Job` performs, for Jobs whose owning CR
/// doesn't record the Job name in status (e.g. `Restore`). Values:
/// [`OP_RESTORE`], [`OP_RESTORE_TARGET`].
pub const OP_LABEL: &str = "kopiur.home-operations.com/op";
/// [`OP_LABEL`] value for a `Restore`'s mover Job.
pub const OP_RESTORE: &str = "restore";
/// [`OP_LABEL`] value for a `Restore`'s operator-created target PVC.
pub const OP_RESTORE_TARGET: &str = "restore-target";

/// Label marking a mover `Job` as an interactive data-plane *session* pod
/// (spawned by `kubectl kopiur browse`/`ls`/`cat`/`download`, not by the
/// operator). Value: [`SESSION_BROWSE`]. Wire-visible: the CLI finds (and
/// reuses) a warm session by this selector, and `session end` deletes by it.
pub const SESSION_LABEL: &str = "kopiur.home-operations.com/session";
/// [`SESSION_LABEL`] value for a read-only browse session.
pub const SESSION_BROWSE: &str = "browse";
/// Label keying a session `Job` to the repository it holds open, as
/// `<kind>-<name>` (e.g. `Repository-nas`). One warm session per repository:
/// the CLI selects on this so two snapshots in the same repository share a pod.
pub const SESSION_REPO_LABEL: &str = "kopiur.home-operations.com/session-repo";

/// Annotation requesting an out-of-band `Maintenance` run NOW (Flux-style
/// reconcile trigger). Value: an RFC3339 timestamp; a NEW timestamp requests a
/// new run (re-applying the same value is a no-op once handled). Usable from
/// bare `kubectl annotate` or `kubectl kopiur maintenance run`.
pub const RUN_REQUESTED_ANNOTATION: &str = "kopiur.home-operations.com/run-requested";
/// Companion annotation selecting the run kind: `quick` (default) or `full`
/// (see `kopiur_api::maintenance::ManualRunMode`).
pub const RUN_MODE_ANNOTATION: &str = "kopiur.home-operations.com/run-mode";

/// Acknowledges an intentional identity-affecting change on UPDATE. Two surfaces
/// share it:
/// - a `SnapshotPolicy`'s own resolved kopia identity (`username@hostname`, or a
///   source's path) — see `ValidationError::IdentityWouldFork`;
/// - a `Repository`/`ClusterRepository`'s `identityDefaults` (`cluster`,
///   `hostnameExpr`, `usernameExpr`), which every consumer policy relying on
///   those defaults re-resolves against on its next reconcile/backup — see
///   `ValidationError::RepositoryIdentityWouldFork`.
///
/// Without it, the webhook **rejects** the edit when it would re-identify
/// (a) policy/(consumer policies) with existing snapshot history, because new
/// snapshots land under a new kopia source: restore/verify/`fromPolicy` resolve
/// the new identity (old history is reachable only via
/// `Restore.spec.source.identity`), while old- and new-lineage `Snapshot` CRs
/// keep competing in the policy's one GFS retention timeline. Any
/// **non-empty** value acknowledges the re-identification for that admission
/// (presence-only, mirroring [`SKIP_SNAPSHOT_CLEANUP_ANNOTATION`]; a specific
/// value isn't required because an edit can change more than one identity
/// component at once, and the operator-resolved string is not something the
/// author can pre-compute). Lives here because the operator and any GitOps/user
/// automation must agree on it byte-for-byte.
pub const ALLOW_IDENTITY_CHANGE_ANNOTATION: &str =
    "kopiur.home-operations.com/allow-identity-change";

/// The API version string for kopiur CRDs (used in mover `TargetRef`s and
/// `kubectl -o name`-style output).
pub const API_VERSION: &str = "kopiur.home-operations.com/v1alpha1";

/// Pod label opting a mover pod into the **azure-workload-identity** mutating
/// webhook: pods carrying `azure.workload.identity/use: "true"` and running as
/// a federated `ServiceAccount` get `AZURE_TENANT_ID`/`AZURE_CLIENT_ID`/
/// `AZURE_FEDERATED_TOKEN_FILE` (and the projected token volume) injected —
/// exactly the env kopia's azure backend binds its credential flags to. Stamped
/// by the operator (and the CLI's browse sessions) on every mover pod for a
/// repository whose azure backend uses `auth.workloadIdentity`. Lives here
/// because the operator and `kubectl kopiur` must agree on it byte-for-byte.
pub const AZURE_WORKLOAD_IDENTITY_LABEL: &str = "azure.workload.identity/use";
/// The [`AZURE_WORKLOAD_IDENTITY_LABEL`] value opting the pod in.
pub const AZURE_WORKLOAD_IDENTITY_LABEL_VALUE: &str = "true";

/// The standard `app.kubernetes.io/managed-by` label key. Stamped on **every**
/// operator-created object (mover Jobs, work-spec ConfigMaps, cache PVC, minted
/// mover SA/RoleBinding, projected credential Secret, CSI VolumeSnapshots) so
/// Argo/Flux recognize them as controller-owned and neither prune nor report them
/// `OutOfSync` (ADR-0005 §14(c)).
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
/// The [`MANAGED_BY_LABEL`] value identifying kopiur-managed objects.
pub const MANAGED_BY_VALUE: &str = "kopiur";

/// kstatus-compliant standard condition types (ADR-0005 §2) so `kubectl wait
/// --for=condition=Ready` and Flux/Argo health checks work natively against every
/// reconciled kopiur CRD.
/// The headline readiness condition.
pub const READY_CONDITION: &str = "Ready";
/// Set `True` while a reconcile is making progress toward Ready.
pub const RECONCILING_CONDITION: &str = "Reconciling";
/// Set `True` when the resource is stuck and won't progress without intervention
/// (mapped from a terminal `ErrorClass::Terminal` failure).
pub const STALLED_CONDITION: &str = "Stalled";

/// `Repository`/`ClusterRepository` condition recording whether a `Maintenance`
/// covers it (ADR §3.7). Wire-visible: GitOps health checks and the kubectl
/// plugin's `status` read it.
pub const MAINTENANCE_CONFIGURED_CONDITION: &str = "MaintenanceConfigured";

/// `Repository`/`ClusterRepository` condition reporting content-index-blob
/// health (ADR-0005 §13). `True` = healthy (count under threshold); `False`
/// with reason `TooManyIndexBlobs` = the index is growing unbounded because
/// maintenance isn't compacting. NON-BLOCKING: the repository stays `Ready` and
/// GitOps health gates are not tripped — it's a degradation warning, not an
/// outage. Wire-visible (the kubectl plugin's `status` reads it).
pub const INDEX_BLOB_HEALTH_CONDITION: &str = "IndexBlobHealth";

/// Default `spec.health.indexBlobWarnThreshold`: the index-blob count above which
/// the reconciler warns that maintenance isn't keeping up. A freshly-compacted
/// repo sits near zero; a wedged-maintenance repo climbs unbounded (a real one
/// reached 1448). Conservative so it only fires when maintenance is clearly
/// behind. Overridable per-repo; `0` disables the warning. Part of the documented
/// API contract, so it lives here rather than in the controller.
pub const DEFAULT_INDEX_BLOB_WARN_THRESHOLD: i64 = 1000;

/// Default catalog re-scan cadence when `spec.catalog.refreshInterval` is unset:
/// how often a `Ready` repository re-lists its kopia snapshots to materialize
/// (and expire) `origin: discovered` `Snapshot` CRs. Part of the documented API
/// contract (field-reference), so it lives here rather than in the controller.
pub const DEFAULT_CATALOG_REFRESH_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(3600);

/// Floor for `spec.catalog.refreshInterval`, enforced at admission. Each re-scan
/// of an object-store repository runs a short mover Job; anything faster than
/// this is Job churn with no operational value.
pub const MIN_CATALOG_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Default `spec.health.probe.interval` when unset: how often an opt-in backend
/// health probe re-connects a `Ready` repository to confirm the kopia repository
/// still exists at the backend. Off by default; only meaningful once
/// `spec.health.probe.enabled: true`. Conservative — a vanished/unreachable
/// repository is rare and the probe runs a short mover Job — so it leans long.
/// Part of the documented API contract, so it lives here, not in the controller.
pub const DEFAULT_HEALTH_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1800);

/// Floor for `spec.health.probe.interval`, enforced at admission. Each probe runs
/// a short mover Job (object-store / volume-backed) or an in-process connect;
/// anything faster than this is Job churn with no operational value. Shares the
/// 30s floor with the catalog re-scan for the same reason.
pub const MIN_HEALTH_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Default `spec.health.probe.failureThreshold`: how many *consecutive* failing
/// probes must accumulate before the loud `RepositoryVanished` / `BackendReachable=False`
/// condition is raised and an event fired. Debounces a single transient blip
/// (an S3 list-after-delete race, a NAS reboot, a credential-rotation moment)
/// from alarming on-call or nudging a destructive manual recreate.
pub const DEFAULT_HEALTH_PROBE_FAILURE_THRESHOLD: i64 = 3;

/// Default `SnapshotSchedule.spec.failedJobsHistoryLimit` when unset: how many
/// `Failed` `Snapshot` CRs from a schedule to retain (the rest are pruned). Bounds
/// failure history so a schedule firing against a persistently-failing precondition
/// or backend doesn't accumulate `Failed` CRs forever. GFS retention applies only to
/// successful snapshots, so this is the *only* bound on failures (ADR-0003). Part of
/// the documented API contract, so it lives here, not in the controller.
pub const DEFAULT_FAILED_JOBS_HISTORY_LIMIT: u32 = 10;

/// The effective failed-history limit: `failedJobsHistoryLimit` when set, else
/// [`DEFAULT_FAILED_JOBS_HISTORY_LIMIT`]. `Some(0)` keeps no failed snapshots.
pub fn effective_failed_jobs_history_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_FAILED_JOBS_HISTORY_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_version_is_group_slash_version() {
        // The duplicated literal must never drift from the canonical pair.
        assert_eq!(API_VERSION, format!("{}/{}", crate::GROUP, crate::VERSION));
    }

    #[test]
    fn well_known_strings_are_group_prefixed() {
        // Finalizers/labels/annotations on the kopiur API surface live under the
        // API group domain; a typo'd prefix would silently break selectors.
        for s in [
            SNAPSHOT_CLEANUP_FINALIZER,
            SKIP_SNAPSHOT_CLEANUP_ANNOTATION,
            ORIGIN_LABEL,
            SNAPSHOT_ID_LABEL,
            REPOSITORY_UID_LABEL,
            CONFIG_LABEL,
            SCHEDULE_LABEL,
            OP_LABEL,
            SESSION_LABEL,
            SESSION_REPO_LABEL,
            RUN_REQUESTED_ANNOTATION,
            RUN_MODE_ANNOTATION,
            ALLOW_IDENTITY_CHANGE_ANNOTATION,
        ] {
            assert!(s.starts_with(crate::GROUP), "{s} must be group-prefixed");
        }
    }
}
