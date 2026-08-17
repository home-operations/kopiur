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

/// Label naming the `SnapshotReplication` CR that minted an
/// `origin: replicated` dest-side copy `Snapshot` — the selector a replication
/// run (and its pruning pass) uses to find its own copies. Stamped at CREATE,
/// alongside [`ORIGIN_LABEL`]/[`SNAPSHOT_ID_LABEL`]/[`REPOSITORY_UID_LABEL`].
/// Lives in `kopiur-api` because the CLI and user automation must agree on it
/// byte-for-byte. (Nothing stamps it yet — reserved by the shared-foundations
/// milestone of #368.)
pub const SNAPSHOT_REPLICATION_LABEL: &str = "kopiur.home-operations.com/snapshot-replication";

/// Label naming the shared CSI `VolumeGroupSnapshot` a fanned-out `Snapshot`
/// stages from — carried by BOTH the member `Snapshot` CRs and the
/// `VolumeGroupSnapshot` object itself.
///
/// This is the group's only join key, which makes it load-bearing rather than
/// decorative. The `VolumeGroupSnapshot` deliberately has **no**
/// ownerReferences (see `io::group_staging`): every candidate owner is either
/// absent for a manual run, long-lived enough that GC never fires, or — for a
/// "leader" member — liable to be pruned while siblings are still restoring
/// from the group's member snapshots. So one label selector answers both "who
/// are my siblings" and "does any live member still need this group", and the
/// reaper fails CLOSED when that read fails.
///
/// Lives in `kopiur-api`, not `kopiur-controller`, because the CLI (to filter a
/// fan-out) and the webhook (to stamp it on a hand-applied member) both need it
/// and neither can depend on the controller crate.
pub const GROUP_LABEL: &str = "kopiur.home-operations.com/group";

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

/// Marks a `Snapshot` the OPERATOR is deleting as part of its own lifecycle,
/// stamped immediately before the delete call. Values: `PrunedBy` annotation
/// values (`retention`, `failed-history`, `policy-cascade`,
/// `replication-retention`). The Snapshot finalizer uses it to
/// distinguish Kopiur's own prunes from external deletions (GC cascades,
/// kubectl, third-party controllers); external destructive deletions are
/// subject to the schedule-cascade guard and the mass-deletion breaker,
/// operator prunes are not. Any unrecognized value is treated as EXTERNAL
/// (fail-safe). Wire-visible: users and tooling may read it on terminating CRs.
pub const PRUNED_BY_ANNOTATION: &str = "kopiur.home-operations.com/pruned-by";

/// Acknowledges a mass-deletion wave on a `Repository`/`ClusterRepository`.
/// Value: an RFC3339 timestamp. A HELD external deletion is released iff its
/// Snapshot's `metadata.deletionTimestamp` <= this value — "I approve what is
/// pending NOW". Deliberately VALUED (unlike the presence-only
/// allow-identity-change ack, consumed at a single admission instant): this
/// annotation is read continuously by the controller, so a presence-only ack
/// left behind (or committed to Git) would disarm the breaker forever. With
/// the timestamp, a stale ack is inert against any LATER wave, nothing ever
/// needs to remove it, and the operator never edits user metadata. The
/// controller clamps the effective value to <= its own now (clock-skew guard);
/// an unparseable value is ignored (Warning event on the repository).
pub const ALLOW_MASS_DELETION_ANNOTATION: &str = "kopiur.home-operations.com/allow-mass-deletion";

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

/// Default `spec.health.probe.enabled` when unset: the periodic backend health
/// probe is ON by default (#345). The probe is the sensor for the repository
/// circuit breaker — with the default `onFailure: Degrade`, a sustained backend
/// failure moves the repository to `Degraded` and pauses backups until a
/// re-connect succeeds — so it must run unless the user explicitly opts out with
/// `enabled: false`. Part of the documented API contract, so it lives here, not
/// in the controller.
pub const DEFAULT_HEALTH_PROBE_ENABLED: bool = true;

/// Default `spec.health.probe.interval` when unset: how often the backend
/// health probe re-connects a `Ready` repository to confirm the kopia repository
/// still exists at the backend. Conservative — a vanished/unreachable
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
/// condition is raised, an event fired, and — under the default
/// `onFailure: Degrade` — the repository moved to `Degraded` (pausing backups).
/// Debounces a single transient blip (an S3 list-after-delete race, a NAS
/// reboot, a credential-rotation moment) from alarming on-call, tripping the
/// breaker, or nudging a destructive manual recreate.
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

/// Default `spec.deletionProtection.threshold` (0 disables). 10 pending
/// external destructive deletions is far above legitimate manual cleanup but
/// far below a tooling-driven cascade (the motivating incident was ~600).
pub const DEFAULT_MASS_DELETION_THRESHOLD: u32 = 10;

/// The effective mass-deletion breaker threshold: `deletionProtection.threshold`
/// when set, else [`DEFAULT_MASS_DELETION_THRESHOLD`]. `Some(0)` disables the breaker.
pub fn effective_mass_deletion_threshold(p: Option<&crate::common::DeletionProtectionSpec>) -> u32 {
    p.and_then(|d| d.threshold)
        .unwrap_or(DEFAULT_MASS_DELETION_THRESHOLD)
}

/// `Repository`/`ClusterRepository` condition: pending external destructive
/// deletions for this repository are at/above the breaker threshold and held.
pub const MASS_DELETION_HELD_CONDITION: &str = "MassDeletionHeld";

/// `reason` for [`MASS_DELETION_HELD_CONDITION`] = `True` on a
/// `Repository`/`ClusterRepository`: pending external destructive deletions for
/// this repository are at/above its breaker threshold.
pub const MASS_DELETION_THRESHOLD_EXCEEDED_REASON: &str = "ThresholdExceeded";

// --- Structural-gate conditions/reasons (shared with `kubectl kopiur doctor`) -
//
// These are the condition `type`/`reason` pairs the reconcilers stamp when work
// is BLOCKED on something only a human can change (a namespace opt-in, a Secret,
// an acknowledgement annotation). They live here — not in `kopiur-controller` —
// because the CLI's `doctor` must recognize them byte-for-byte; the typed
// registry in [`crate::gates`] is the shared enumeration built from them, and
// the controller re-exports each name so its call sites are unchanged.

/// Namespace annotation a cluster admin sets to allow elevated (root/privileged)
/// movers in that namespace (ADR §4.11/§G16). Without it, a `SnapshotPolicy` whose
/// `spec.mover` requests privilege is refused — a tenant could otherwise reuse the
/// minted mover ServiceAccount at that privilege. Mirrors VolSync's
/// `volsync.backube/privileged-movers`.
pub const PRIVILEGED_MOVERS_ANNOTATION: &str = "kopiur.home-operations.com/privileged-movers";
/// `Snapshot`/`Restore` condition surfaced when a privileged mover is requested in a
/// namespace that has not opted in — `False` carries the actionable message.
pub const MOVER_PERMITTED_CONDITION: &str = "MoverPermitted";
/// `reason`/Event reason for [`MOVER_PERMITTED_CONDITION`] = `False`.
pub const PRIVILEGED_MOVER_NOT_PERMITTED_REASON: &str = "PrivilegedMoverNotPermitted";

/// `SnapshotSchedule` condition recording whether the schedule is able to fire
/// its next slot. Set `False` (with [`BLOCKED_ON_UNREADABLE_RUN_REASON`]) when
/// the concurrency gate is held by a `Snapshot` whose `status.phase` this build
/// cannot interpret.
pub const SCHEDULE_RUNNABLE_CONDITION: &str = "ScheduleRunnable";
/// `reason`/Event reason for [`SCHEDULE_RUNNABLE_CONDITION`] = `False`: a
/// previous run of this schedule sits at a phase string written by a NEWER
/// kopiur, so this build can never observe it reach a terminal phase. Under the
/// default `concurrencyPolicy: Forbid` that stops the schedule permanently, and
/// nothing about the `SnapshotSchedule` itself would otherwise say so — the
/// silent-wedge shape of #359, one kind removed.
pub const BLOCKED_ON_UNREADABLE_RUN_REASON: &str = "BlockedOnUnreadableRun";

/// `Snapshot` condition recording whether its repository accepts writes (§11). Set
/// `False` (with [`REPOSITORY_READ_ONLY_REASON`]) when a backup is refused because
/// the repository is `mode: ReadOnly`.
pub const REPOSITORY_WRITABLE_CONDITION: &str = "RepositoryWritable";
/// `reason`/Event reason when a backup or maintenance is refused on a `ReadOnly`
/// repository (ADR-0005 §11).
pub const REPOSITORY_READ_ONLY_REASON: &str = "RepositoryReadOnly";

/// `Snapshot`/`Restore` condition surfaced when the mover Job's credential Secret is
/// absent from the workload namespace — `False` carries the actionable message
/// (which Secret, which namespace, why, and how to fix). ADR §4.12.
pub const CREDENTIALS_AVAILABLE_CONDITION: &str = "CredentialsAvailable";
/// `reason`/Event reason for [`CREDENTIALS_AVAILABLE_CONDITION`] = `False`.
pub const MISSING_CREDENTIALS_REASON: &str = "MissingCredentialsSecret";
/// `reason`/Event reason for [`CREDENTIALS_AVAILABLE_CONDITION`] = `False` when
/// the missing dependency is the **workload-identity ServiceAccount** the
/// backend's `auth.workloadIdentity` names (the user creates it; kopiur never
/// does — its cloud annotations are the user's federation contract).
pub const MISSING_SERVICE_ACCOUNT_REASON: &str = "MissingServiceAccount";
/// `reason`/Event reason for [`CREDENTIALS_AVAILABLE_CONDITION`] = `False` when
/// the missing dependency is the backend's `tls.caBundleRef` **ConfigMap** (or
/// its key): the PEM CA bundle the mover needs to verify a private-CA S3
/// endpoint. The user creates it; a ConfigMap that never appears never
/// self-heals, so this is a structural gate
/// ([`crate::gates::MISSING_CA_BUNDLE_GATE`]), not just a transient retry.
pub const MISSING_CA_BUNDLE_REASON: &str = "MissingCaBundle";
/// Default key within a `tls.caBundleRef` ConfigMap when `key` is unset — the
/// conventional filename cert-manager and trust-manager emit CA bundles under.
/// Lives here (not in the controller) because the kubectl-plugin resolves the
/// same reference client-side and must agree on the default (centralize-config).
pub const DEFAULT_CA_BUNDLE_KEY: &str = "ca.crt";

/// `SnapshotPolicy` condition recording whether every repository the policy
/// targets is `Ready`. Set `False` (with [`REPOSITORY_NOT_READY_REASON`]) when
/// at least one referenced `Repository`/`ClusterRepository` is not `Ready`:
/// backups, retention, adoption and verification against the not-ready
/// subset are deferred (the ready subset keeps processing), and nothing about
/// the policy's phase-less surface would otherwise say so — the silent-wedge
/// shape of #359 for the recipe kind. Structural
/// ([`crate::gates::POLICY_REPOSITORY_NOT_READY_GATE`]): a repository that
/// never recovers never self-heals this condition.
pub const REPOSITORIES_READY_CONDITION: &str = "RepositoriesReady";
/// `reason` for [`REPOSITORIES_READY_CONDITION`] = `False` — the same string
/// the `Snapshot` reconciler stamps on a child parked behind a not-Ready
/// repository (one string, both surfaces; the controller re-exports it).
pub const REPOSITORY_NOT_READY_REASON: &str = "RepositoryNotReady";

/// `SnapshotSchedule` condition recording whether a fired slot minted its full
/// members × repositories fan-out. `True` (with [`FANOUT_TOO_LARGE_REASON`])
/// = the cross-product exceeded the fan-out cap and the slot was SKIPPED — and
/// it will keep skipping every slot until the selector is narrowed or the
/// repository list shrunk, which is squarely "needs a human". Structural
/// ([`crate::gates::SCHEDULE_FANOUT_CAPPED_GATE`], promoted here by the #368
/// M10 gates/doctor checklist from a controller-internal condition).
pub const SCHEDULE_FANOUT_CAPPED_CONDITION: &str = "FanoutCapped";
/// `reason` for [`SCHEDULE_FANOUT_CAPPED_CONDITION`] = `True`.
pub const FANOUT_TOO_LARGE_REASON: &str = "FanoutTooLarge";

/// `Snapshot` condition recording whether the backup's DIRECT source PVC
/// (`spec.sources[].pvc`) exists. Set `False` (with
/// [`SOURCE_PVC_MISSING_REASON`]) when the PVC named by the recipe is absent
/// at launch time: the backup parks (`phase: Pending`) and, after the
/// controller's deadline, fails terminally — a PVC that never reappears never
/// self-heals, the silent-wedge shape of #359. Structural
/// ([`crate::gates::SOURCE_PVC_MISSING_GATE`]); only the direct source PVC
/// earns this gate — a vanished operator-staged claim is a restage race and
/// stays a transient retry.
pub const SOURCE_PVC_AVAILABLE_CONDITION: &str = "SourcePvcAvailable";
/// `reason`/Event reason for [`SOURCE_PVC_AVAILABLE_CONDITION`] = `False`.
pub const SOURCE_PVC_MISSING_REASON: &str = "SourcePvcMissing";

/// `Snapshot` condition: this deletion is HELD by the mass-deletion breaker
/// (`Repository`/`ClusterRepository` `spec.deletionProtection.threshold`)
/// until acknowledged via [`ALLOW_MASS_DELETION_ANNOTATION`] on the
/// repository.
pub const DELETION_HELD_CONDITION: &str = "DeletionHeld";
/// `reason` for [`DELETION_HELD_CONDITION`] = `True`.
pub const MASS_DELETION_BREAKER_REASON: &str = "MassDeletionBreaker";

/// `Repository`/`ClusterRepository` condition reporting the state of
/// `spec.seed` — initializing a brand-new repository from an existing replica
/// (issue #380).
///
/// `True` means the repository holds its seeded content (reason
/// [`SEEDED_REASON`]) or never needed seeding because it was already
/// initialized (reason [`ALREADY_INITIALIZED_REASON`]). `False` means the seed
/// is in flight ([`SEEDING_REASON`]), parked on a source that is not usable yet
/// ([`WAITING_FOR_SEED_SOURCE_REASON`]), or failed (the mover's failure class).
///
/// Wire-visible: GitOps health checks, `kubectl kopiur status`/`doctor` and
/// user automation read it, so it lives in `kopiur-api` beside the other
/// contract strings rather than controller-side.
pub const SEEDED_CONDITION: &str = "Seeded";
/// `reason` for [`SEEDED_CONDITION`] = `True` when this repository's content
/// was actually copied in from `spec.seed.from`.
pub const SEEDED_REASON: &str = "Seeded";
/// `reason` for [`SEEDED_CONDITION`] = `True` when `spec.seed` was a standing
/// no-op: the repository was already initialized, so nothing was copied. This
/// is the steady state of a seed block left in a GitOps manifest forever.
pub const ALREADY_INITIALIZED_REASON: &str = "AlreadyInitialized";
/// `reason` for [`SEEDED_CONDITION`] = `False` while the seeding bootstrap Job
/// is in flight. Bounded by `spec.seed.failurePolicy` (default 24h), but a seed
/// legitimately runs for hours, so anything reporting on a repository must be
/// able to say "seeding" rather than "stuck".
pub const SEEDING_REASON: &str = "Seeding";
/// `reason` for [`SEEDED_CONDITION`] = `False` when a migrate-mode seed is
/// parked because its source `Repository`/`ClusterRepository` is not `Ready`
/// (or does not exist). The repository stays `Pending` and re-checks until the
/// source comes up — an out-of-band change nothing in kopiur can make.
pub const WAITING_FOR_SEED_SOURCE_REASON: &str = "WaitingForSeedSource";

// The FAILURE reasons for [`SEEDED_CONDITION`] = `False`. Each is byte-identical
// to the sentinel `kopia_error_class` the seeding mover writes
// (`kopiur_mover::bootstrap::SEED_*_CLASS`) — one vocabulary across the mover's
// result, the condition, the Warning Event and the CLI. They live HERE, not in
// the mover, because `gates.rs` registers them and `kopiur-api` cannot depend on
// `kopiur-mover`; the equality is pinned by the controller's
// `io::tests::every_seed_class_maps_to_a_typed_failure_and_routes_by_retryability`,
// which is the one crate that sees both sides.
//
// Registering ALL of them matters: a `Seeded=False` reason with no registry row
// trips `StructuralGate::trips` but matches no row, and `kubectl kopiur doctor`
// then reports it as "the operator is newer than the plugin" — a false diagnosis
// in exactly the disaster-recovery flow these reasons exist for.

/// `reason` for [`SEEDED_CONDITION`] = `False` when the seed SOURCE answered but
/// holds no kopia repository (a mis-pointed bucket/prefix, or a mirror that was
/// never written). Retried automatically.
pub const SEED_SOURCE_NOT_FOUND_REASON: &str = "SeedSourceNotFound";
/// `reason` for [`SEEDED_CONDITION`] = `False` when the seed source IS a kopia
/// repository but holds zero snapshots and `spec.seed.allowEmptySource` is
/// `false`. Retried automatically, so a mirror that fills up later seeds itself.
pub const SEED_SOURCE_EMPTY_REASON: &str = "SeedSourceEmpty";
/// `reason` for [`SEEDED_CONDITION`] = `False` when a migrate-mode seed's
/// post-verify found snapshots missing at the destination (`kopia snapshot
/// migrate` exits 0 on a per-source failure, so the destination listing is the
/// only honest success signal). The next attempt resumes the copy.
pub const SEED_INCOMPLETE_REASON: &str = "SeedIncomplete";
/// `reason` for [`SEEDED_CONDITION`] = `False` when a seed was armed and the
/// repository ended the bootstrap holding ZERO snapshots — an attempt that
/// initialized the backend and then died. The next attempt resumes the copy;
/// nothing at the backend should be deleted.
pub const SEED_LEFT_EMPTY_REASON: &str = "SeedLeftEmpty";
/// `reason` for [`SEEDED_CONDITION`] = `False` when the running mover image
/// predates `spec.seed`: it dropped the unknown field, fell into the create
/// fallback, and initialized an EMPTY repository. Terminal — only an image
/// upgrade changes it.
pub const SEED_MOVER_TOO_OLD_REASON: &str = "MoverImageTooOldForSeed";

/// Every `Seeded=False` reason that means "the last seed attempt FAILED", as
/// opposed to the park/progress reasons ([`WAITING_FOR_SEED_SOURCE_REASON`],
/// [`SEEDING_REASON`]).
///
/// Exists so a writer can ask "has a previous attempt already recorded a
/// failure here?" without restating the list — the seeding-progress writer uses
/// it to leave a recorded failure standing rather than overwriting it with
/// `Seeding` on every ~2-minute retry, which would make every retry cycle a
/// fresh status transition (and so a fresh Event and metric increment).
pub const SEED_FAILURE_REASONS: &[&str] = &[
    SEED_SOURCE_NOT_FOUND_REASON,
    SEED_SOURCE_EMPTY_REASON,
    SEED_INCOMPLETE_REASON,
    SEED_LEFT_EMPTY_REASON,
    SEED_MOVER_TOO_OLD_REASON,
];

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
            SNAPSHOT_REPLICATION_LABEL,
            GROUP_LABEL,
            OP_LABEL,
            SESSION_LABEL,
            SESSION_REPO_LABEL,
            RUN_REQUESTED_ANNOTATION,
            RUN_MODE_ANNOTATION,
            ALLOW_IDENTITY_CHANGE_ANNOTATION,
            PRUNED_BY_ANNOTATION,
            ALLOW_MASS_DELETION_ANNOTATION,
            PRIVILEGED_MOVERS_ANNOTATION,
        ] {
            assert!(s.starts_with(crate::GROUP), "{s} must be group-prefixed");
        }
    }
}
