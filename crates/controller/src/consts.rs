//! Controller-internal string constants: event reasons/actions, single-flight
//! labels, deadlines (ADR §4.5).
//!
//! The *wire-contract* strings (finalizer, origin/config/dedup labels,
//! `managed-by`, kstatus condition types, API version) live in
//! [`kopiur_api::consts`] so external tooling shares one definition; they are
//! re-exported here so controller call sites keep their existing import paths.

pub use kopiur_api::consts::{
    API_VERSION, CONFIG_LABEL, INDEX_BLOB_HEALTH_CONDITION, MAINTENANCE_CONFIGURED_CONDITION,
    MANAGED_BY_LABEL, MANAGED_BY_VALUE, OP_LABEL, OP_RESTORE, OP_RESTORE_TARGET, ORIGIN_LABEL,
    READY_CONDITION, RECONCILING_CONDITION, REPOSITORY_UID_LABEL, RUN_MODE_ANNOTATION,
    RUN_REQUESTED_ANNOTATION, SCHEDULE_LABEL, SKIP_SNAPSHOT_CLEANUP_ANNOTATION,
    SNAPSHOT_CLEANUP_FINALIZER, SNAPSHOT_ID_LABEL, STALLED_CONDITION,
};

/// `Snapshot` condition recording whether its repository accepts writes (§11). Set
/// `False` (with [`REPOSITORY_READ_ONLY_REASON`]) when a backup is refused because
/// the repository is `mode: ReadOnly`.
pub const REPOSITORY_WRITABLE_CONDITION: &str = "RepositoryWritable";
/// `reason`/Event reason when a backup or maintenance is refused on a `ReadOnly`
/// repository (ADR-0005 §11).
pub const REPOSITORY_READ_ONLY_REASON: &str = "RepositoryReadOnly";

/// `reason` when a backup is held in `Pending` because its referenced repository
/// is not `Ready` (backend unreachable). Mirrors the readiness gate Maintenance,
/// `SnapshotPolicy`, and `RepositoryReplication` already apply.
pub const REPOSITORY_NOT_READY_REASON: &str = "RepositoryNotReady";

/// `reason` when a backup is held in `Pending` (then `Failed` after the timeout)
/// because a `SnapshotPolicy.spec.preflight` check is not satisfied. The backup
/// never launches until every preflight check passes.
pub const PREFLIGHT_FAILED_REASON: &str = "PreflightFailed";
/// `reason` when a preflight-gated backup is held in `Pending` because the
/// `Maintenance` informer cache has not finished its initial sync yet (so
/// maintenance recency can't be trusted). Surfaced so a never-syncing informer
/// (e.g. missing RBAC on `Maintenance`) is diagnosable instead of a silent stall.
pub const PREFLIGHT_WAITING_REASON: &str = "WaitingForPreflightData";
/// Default `spec.preflight.timeout` when unset: how long a `Snapshot` is held in
/// `Pending` while a preflight check is unsatisfied before it transitions to
/// `Failed`. Bounded so scheduled backups don't pile up `Pending` CRs.
pub const DEFAULT_PREFLIGHT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// Default `spec.staging.timeout` when unset: how long a staged `VolumeSnapshot`
/// may take to become `readyToUse` (from its creation) before the backup is
/// failed. Bounded so a broken CSI driver can't hold a `Snapshot` `Pending`
/// forever and silently starve a `concurrencyPolicy: Forbid` schedule; a
/// transient `status.error` during the wait never fails staging on its own.
pub const DEFAULT_STAGING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// In-container mount path for an inline-NFS backup *source* whose server-side
/// export is the NFSv4 pseudo-root (`/`). The export's server path and the
/// container mount path are independent; reusing `/` as the mount path would
/// mount the volume over the container rootfs and the pod fails to start
/// (`error mounting ... to rootfs at "/": mountpoint ... is on the top of
/// rootfs`). kopia snapshots whatever is mounted here.
pub const NFS_SOURCE_MOUNT_PATH: &str = "/nfs";

/// Standard component label. `maintenance` marks the mover Jobs the `Maintenance`
/// reconciler spawns, so it can enforce single-flight (at most one maintenance
/// Job per repository at a time, G3) via a label selector.
pub const COMPONENT_LABEL: &str = "app.kubernetes.io/component";
/// `COMPONENT_LABEL` value for maintenance mover Jobs.
pub const MAINTENANCE_COMPONENT: &str = "maintenance";
/// Label tying a maintenance Job back to its owning `Maintenance` CR (the
/// single-flight selector is `COMPONENT_LABEL`=maintenance + this = CR name).
pub const MAINTENANCE_INSTANCE_LABEL: &str = "kopiur.home-operations.com/maintenance";
/// Annotation on a maintenance Job recording the scheduled slot it runs (RFC3339;
/// not a valid *label* value because of the colons). Mirrors the upstream
/// `batch.kubernetes.io/cronjob-scheduled-timestamp` (G9).
pub const MAINTENANCE_SLOT_ANNOTATION: &str = "kopiur.home-operations.com/maintenance-slot";

/// `COMPONENT_LABEL` value for verification mover Jobs (ADR-0005 §4).
pub const VERIFY_COMPONENT: &str = "verify";
/// Label tying a verification Job back to its owning `SnapshotPolicy` (single-flight
/// selector: `COMPONENT_LABEL`=verify + this = policy name).
pub const VERIFY_INSTANCE_LABEL: &str = "kopiur.home-operations.com/verify";
/// Annotation on a verification Job recording the scheduled slot it runs (RFC3339).
pub const VERIFY_SLOT_ANNOTATION: &str = "kopiur.home-operations.com/verify-slot";

/// `COMPONENT_LABEL` value for replication mover Jobs (ADR-0005 §13(d)).
pub const REPLICATION_COMPONENT: &str = "replication";
/// Label tying a replication Job back to its owning `RepositoryReplication`.
pub const REPLICATION_INSTANCE_LABEL: &str = "kopiur.home-operations.com/replication";
/// Annotation on a replication Job recording the scheduled slot it runs (RFC3339).
pub const REPLICATION_SLOT_ANNOTATION: &str = "kopiur.home-operations.com/replication-slot";

/// Annotation a `Snapshot` stamps (RFC3339) on its repository when a backup fails,
/// requesting an immediate connectivity re-probe. Honored once via
/// `status.lastReverifyAt`; rate-limited so a wave of failures forces one re-probe.
pub const REVERIFY_REQUESTED_ANNOTATION: &str = "kopiur.home-operations.com/reverify-requested-at";

/// Condition reason when a `Maintenance` (managed or external) covers the repo.
pub const MAINTENANCE_CONFIGURED_REASON: &str = "MaintenanceConfigured";
/// `action` for the maintenance-configuration check Event.
pub const CHECK_MAINTENANCE_ACTION: &str = "CheckMaintenance";
/// Condition reason when `spec.maintenance.enabled: false` and no external
/// `Maintenance` covers the repo — a deliberate opt-out, surfaced informationally
/// (no Warning event).
pub const MAINTENANCE_DISABLED_REASON: &str = "MaintenanceDisabled";
/// Event + condition reason when a `ClusterRepository`'s managed `Maintenance`
/// cannot be placed: neither `spec.maintenance.namespace` nor the operator
/// namespace (`KOPIUR_NAMESPACE`) is set. A real misconfiguration, so it warns.
pub const MAINTENANCE_NAMESPACE_UNRESOLVED_REASON: &str = "MaintenanceNamespaceUnresolved";
/// Event + condition reason when maintenance is ENABLED but the operator could not apply
/// its managed `Maintenance` (a failed server-side apply, or an un-buildable owner ref).
/// Distinct from [`MAINTENANCE_DISABLED_REASON`], which is a deliberate opt-out: reporting
/// a transient apply failure as "you disabled maintenance" pointed operators at entirely
/// the wrong knob (#231). Warns, and is retried on every reconcile.
pub const MAINTENANCE_APPLY_FAILED_REASON: &str = "MaintenanceApplyFailed";

/// Status condition `type` recording the outcome of an object-store repository
/// bootstrap Job (connect/create). `True` once the repository is reachable;
/// `False` carries the kopia error class + message so a failure is actionable.
pub const REPOSITORY_BOOTSTRAPPED_CONDITION: &str = "Bootstrapped";

/// `activeDeadlineSeconds` on the object-store bootstrap Job. A bootstrap whose
/// pods never schedule (e.g. a missing mover ServiceAccount, an image-pull
/// failure) otherwise never gets a `Failed` condition, so the controller never
/// finalizes and the repository hangs `Initializing` with no Event. The deadline
/// forces the Job terminal-`Failed` so `finalize_*` runs and surfaces a Warning.
/// Sized comfortably under the e2e Event-publish budget (180s).
pub const BOOTSTRAP_JOB_DEADLINE_SECS: i64 = 120;

// A repository connect/create (bootstrap) failure is surfaced as a Warning Event
// whose `reason` is the kopia error class itself (`KopiaErrorClass::as_str`, e.g.
// `AccessDenied`/`PermissionDenied`) so it matches the `Bootstrapped=False`
// condition reason and is machine-readable. Only the Event `action` (the
// remediation hint) is a controller-side constant:

/// `action` for credential-class failures (`AccessDenied`/`AuthFailure`): check
/// the repository credentials Secret and bucket/path grants.
pub const CHECK_CREDENTIALS_ACTION: &str = "CheckCredentials";

/// Machine-readable `reason` (condition + Warning Event) when a bootstrap Job
/// reaches a terminal/failed state but wrote **no** structured result — the mover
/// pod crashed, was evicted, hit its [`BOOTSTRAP_JOB_DEADLINE_SECS`] deadline, or
/// never scheduled (e.g. a missing mover ServiceAccount). Distinct from a kopia
/// error class so the failure mode is not silently conflated with a backend
/// rejection ([`crate::io::BootstrapFailure`]).
pub const BOOTSTRAP_JOB_FAILED_REASON: &str = "BootstrapJobFailed";

/// [`OP_LABEL`] value for a populator `Restore`'s prime PVC and populate mover Job
/// (distinct from the direct-target `restore` Jobs). ADR-0005 §9.
pub const OP_RESTORE_POPULATE: &str = "restore-populate";
/// `Restore` Ready reason once a populator restored its snapshot and rebound the volume.
pub const RESTORE_POPULATED_REASON: &str = "RestoreSucceeded";
/// `Restore` Ready reason when a populator's claiming PVC is ALREADY bound, so there is
/// nothing to populate: a CSI volume-populator can only hand a volume to an unbound claim.
/// A truthful terminal no-op — no prime PVC, no mover run (#233).
pub const RESTORE_TARGET_ALREADY_BOUND_REASON: &str = "TargetAlreadyBound";
/// Warning Event reason when the populator reaps leftover populate artifacts (a prime PVC
/// and/or its mover Job) that can never be handed over because the claiming PVC is already
/// bound. Pre-0.8 these leaked, each holding a full copy of the restored data (#233).
pub const ORPHANED_PRIME_REAPED_REASON: &str = "OrphanedPrimePvcReaped";
/// `action` for the already-bound no-op / orphan-reap Events: to actually restore into the
/// claim, delete it and let it be recreated (keeping its `dataSourceRef`).
pub const RECREATE_CLAIM_TO_RESTORE_ACTION: &str = "RecreateClaimToRestore";

/// `Snapshot`/`Restore` condition surfaced when the mover Job's credential Secret is
/// absent from the workload namespace — `False` carries the actionable message
/// (which Secret, which namespace, why, and how to fix). ADR §4.12.
pub const CREDENTIALS_AVAILABLE_CONDITION: &str = "CredentialsAvailable";
/// `Snapshot` condition tracking kopia-side pin reconciliation (ADR-0005 §13(c)).
/// `True`/`False` mirrors `status.pinned` once a SnapshotPin mover Job ran;
/// `Unknown` with reason `PinJobRunning` while one is in flight. Named for the
/// state (like `Ready`/`LeaseOwned`). Doubles as the durable "a pin mover was
/// ever spawned" marker that gates the per-reconcile pin-Job lookup — never
/// remove it once set.
pub const PINNED_CONDITION: &str = "Pinned";
/// `reason` for [`PINNED_CONDITION`] = `False`: the SnapshotPin mover Job failed.
pub const PIN_JOB_FAILED_REASON: &str = "PinJobFailed";
/// Annotation stamped on a `{name}-pin` mover Job recording the pin state it
/// was spawned to APPLY (`"true"` = pin, `"false"` = unpin). The reconciler
/// consumes a terminal pin Job by this direction — never by the currently
/// desired `spec.pin` — so a stale Job can't satisfy the opposite toggle and a
/// pin that completed after a mid-flight spec flip is still recorded.
pub const PIN_TARGET_ANNOTATION: &str = "kopiur.home-operations.com/pin-target";
/// `reason`/Event reason for [`CREDENTIALS_AVAILABLE_CONDITION`] = `False`.
pub const MISSING_CREDENTIALS_REASON: &str = "MissingCredentialsSecret";
/// `reason`/Event reason for [`CREDENTIALS_AVAILABLE_CONDITION`] = `False` when
/// the missing dependency is the **workload-identity ServiceAccount** the
/// backend's `auth.workloadIdentity` names (the user creates it; kopiur never
/// does — its cloud annotations are the user's federation contract).
pub const MISSING_SERVICE_ACCOUNT_REASON: &str = "MissingServiceAccount";
/// `reason` for [`CREDENTIALS_AVAILABLE_CONDITION`] = `True` when the operator
/// supplied the credential Secret(s) itself via projection (opt-in
/// `spec.credentialProjection`), rather than the user pre-creating them.
pub const CREDENTIALS_PROJECTED_REASON: &str = "Projected";
/// Annotation stamped on a projected credential Secret recording its source
/// (`<namespace>/<name>`), so an operator can see a copy is kopiur-managed and
/// where it came from. Paired with the `app.kubernetes.io/managed-by=kopiur` +
/// `app.kubernetes.io/component=credentials` labels.
pub const PROJECTED_FROM_ANNOTATION: &str = "kopiur.home-operations.com/projected-from";
/// Label marking a projected credential Secret as a **stable per-CR** copy
/// (value [`CREDS_SCOPE_CR`]), i.e. named `{cr-prefix}-creds-{idx}` and
/// refreshed in place on every run. Its ABSENCE on a Secret that carries
/// [`PROJECTED_FROM_ANNOTATION`] identifies a legacy per-run copy (pre-#231
/// operator versions named copies after per-slot mover Jobs, accumulating one
/// per run forever) — exactly what the periodic sweep reaps.
pub const CREDS_SCOPE_LABEL: &str = "kopiur.home-operations.com/creds-scope";
/// The `app.kubernetes.io/component` value on projected credential Secrets.
pub const CREDS_COMPONENT: &str = "credentials";
/// The [`CREDS_SCOPE_LABEL`] value for stable per-CR projected copies.
pub const CREDS_SCOPE_CR: &str = "cr";

/// Helm value (chart `values.yaml`) that grants the operator `secrets` create/patch
/// so credential projection (`spec.credentialProjection`) works. Surfaced verbatim in
/// the actionable 403 error when a projection write is forbidden, so the message and
/// the chart never drift. See `deploy/helm/kopiur/templates/{role,clusterrole}.tpl`.
pub const CREDENTIAL_PROJECTION_FLAG: &str = "features.credentialProjection.enabled";
/// Helm value that grants the operator `secrets` create/patch/delete so the kopia
/// web-UI server (`spec.server`) works (generated-auth Secret + cross-namespace
/// credentials mirror + teardown delete). Surfaced verbatim in the actionable 403
/// error when a server Secret write is forbidden.
pub const KOPIA_UI_FLAG: &str = "features.kopiaUi.enabled";

/// Namespace annotation a cluster admin sets to allow elevated (root/privileged)
/// movers in that namespace (ADR §4.11/§G16). Without it, a `SnapshotPolicy` whose
/// `spec.mover` requests privilege is refused — a tenant could otherwise reuse the
/// minted mover ServiceAccount at that privilege. Mirrors VolSync's
/// `volsync.backube/privileged-movers`.
pub const PRIVILEGED_MOVERS_ANNOTATION: &str = "kopiur.home-operations.com/privileged-movers";
/// `Snapshot` condition surfaced when a privileged mover is requested in a namespace
/// that has not opted in — `False` carries the actionable message.
pub const MOVER_PERMITTED_CONDITION: &str = "MoverPermitted";
/// `reason`/Event reason for [`MOVER_PERMITTED_CONDITION`] = `False`.
pub const PRIVILEGED_MOVER_NOT_PERMITTED_REASON: &str = "PrivilegedMoverNotPermitted";
/// Event `action` (remediation hint) for a refused privileged mover.
pub const ALLOW_PRIVILEGED_MOVER_ACTION: &str = "AnnotateNamespaceForPrivilegedMovers";
/// `Snapshot` condition for CSI source staging (`copyMethod: Snapshot`/`Clone`,
/// ADR §3.3): `True` once the staged VolumeSnapshot/PVC is ready for the mover;
/// `False` while waiting (reason [`STAGING_WAITING_REASON`]) or on a preflight
/// failure (the `io::staging::REASON_*` tokens: stack/class missing, snapshot error).
pub const SOURCE_STAGED_CONDITION: &str = "SourceStaged";
/// `reason` for [`SOURCE_STAGED_CONDITION`] = `True`.
pub const SOURCE_STAGED_REASON: &str = "SourceStaged";
/// `reason` for [`SOURCE_STAGED_CONDITION`] = `False` while the VolumeSnapshot is
/// still becoming `readyToUse` (a transient, requeued wait — not a failure).
pub const STAGING_WAITING_REASON: &str = "WaitingForVolumeSnapshot";
/// `reason` for [`SOURCE_STAGED_CONDITION`] = `False` while the staged PVC (on an
/// `Immediate`-binding StorageClass) is still binding — the CSI restore/clone from
/// the source is provisioning. A transient, requeued wait bounded by the staging
/// deadline; the terminal counterpart is `io::staging::REASON_STAGED_PVC_BIND_TIMEOUT`.
pub const STAGED_PVC_BINDING_REASON: &str = "WaitingForStagedPvcBind";
/// Event `action` (remediation hint) for a staging preflight failure: install the
/// CSI snapshot stack / VolumeSnapshotClass, or set `copyMethod: Direct`.
pub const FIX_SNAPSHOT_STACK_ACTION: &str = "InstallSnapshotStackOrUseDirect";
/// `Snapshot` condition reporting whether the mover's resolved securityContext can read
/// the backup **source** PVC (a securityContext-only heuristic; the mover's runtime
/// readability preflight is the authoritative check). `True` = provably compatible (root
/// mover / exact-UID match); `Unknown` = undecidable from the spec (the common case);
/// `False` = a near-certain mismatch (carries the advisory remedy). Warn-only; never blocks.
pub const SECURITY_CONTEXT_COMPATIBLE_CONDITION: &str = "SecurityContextCompatible";
/// `reason` for [`SECURITY_CONTEXT_COMPATIBLE_CONDITION`] = `True` (the positive confirmation
/// — the only state the reconcile heuristic ever sets).
pub const SECURITY_CONTEXT_COMPATIBLE_REASON: &str = "SecurityContextCompatible";
/// `reason`/Event reason for [`SECURITY_CONTEXT_COMPATIBLE_CONDITION`] = `False` when a backup
/// COMPLETED but kopia (under an ignore-file-errors policy) excluded unreadable source entries
/// — the *certain*, post-run signal that the snapshot is incomplete (`status.stats.filesFailed`).
/// This is the only thing that ever sets the condition `False`.
pub const SNAPSHOT_INCOMPLETE_REASON: &str = "SnapshotIncompleteUnreadableEntries";
/// Event `action` (remediation hint) for a likely securityContext mismatch: match the
/// mover to the workload via `inheritSecurityContextFrom.pvcConsumer` or a matching UID.
pub const MATCH_WORKLOAD_SECURITY_CONTEXT_ACTION: &str = "MatchWorkloadSecurityContext";
/// `Restore` condition reporting whether the *future* consumer of the restore target PVC
/// will be able to read what the mover writes (a securityContext-only heuristic; no runtime
/// layer exists for restore since the consumer may not exist yet). Same tri-state semantics
/// as [`SECURITY_CONTEXT_COMPATIBLE_CONDITION`]; warn-only.
pub const RESTORE_SECURITY_CONTEXT_COMPATIBLE_CONDITION: &str = "RestoreSecurityContextCompatible";
/// `Snapshot` condition for `spec.hooks` execution (ADR §4.8) — `False` carries
/// the failing hook's index, form, and actionable cause.
pub const HOOKS_SUCCEEDED_CONDITION: &str = "HooksSucceeded";
/// Event `action` (remediation hint) for an aborting hook failure.
pub const FIX_HOOK_ACTION: &str = "FixHookOrSetContinueOnFailure";

/// `SnapshotPolicy` warn-only condition: the deep-verify scratch `storageClassName`
/// (set on `verification.deep` or inherited from `moverDefaults.scratch`) is a silent
/// no-op because no effective `capacity` is set — an `emptyDir` has no StorageClass.
/// `True` = ignored, `False` = honored (the default consistent state). Deep verify
/// still runs (on an `emptyDir`), so this never blocks.
pub const SCRATCH_STORAGE_CLASS_IGNORED_CONDITION: &str = "ScratchStorageClassIgnored";
/// `reason`/Event reason for [`SCRATCH_STORAGE_CLASS_IGNORED_CONDITION`] = `True`.
pub const SCRATCH_STORAGE_CLASS_IGNORED_REASON: &str = "StorageClassIgnored";
/// `reason` for [`SCRATCH_STORAGE_CLASS_IGNORED_CONDITION`] = `False`.
pub const SCRATCH_STORAGE_CLASS_HONORED_REASON: &str = "StorageClassHonored";
/// Event `action` (remediation hint) for the scratch storage-class no-op: set a
/// capacity so a sized PVC is provisioned.
pub const SET_SCRATCH_CAPACITY_ACTION: &str = "SetScratchCapacity";
/// `action` for a `PermissionDenied` failure: make the repository path/PVC
/// writable by the operator's UID.
pub const CHECK_PERMISSIONS_ACTION: &str = "CheckPermissions";
/// `action` for any other backend failure: check the backend configuration.
pub const CHECK_BACKEND_ACTION: &str = "CheckBackend";

/// `SnapshotSchedule` warn-only condition: the schedule inherits its cron timezone
/// from its target policies' repository `scheduleDefaults.timezone`, but the matched
/// policies' repositories **disagree** on the zone, so the controller fell back to
/// UTC. `True` = ambiguous (UTC in effect), `False` = resolved unambiguously (the
/// default consistent state). Never blocks scheduling — it recommends setting an
/// explicit `spec.schedule.timezone`.
pub const SCHEDULE_TIMEZONE_AMBIGUOUS_CONDITION: &str = "TimezoneDefaultAmbiguous";
/// `reason` for [`SCHEDULE_TIMEZONE_AMBIGUOUS_CONDITION`] = `True`.
pub const SCHEDULE_TIMEZONE_AMBIGUOUS_REASON: &str = "RepositoryDefaultsDisagree";
/// `reason` for [`SCHEDULE_TIMEZONE_AMBIGUOUS_CONDITION`] = `False`.
pub const SCHEDULE_TIMEZONE_RESOLVED_REASON: &str = "TimezoneResolved";

/// Machine-readable `reason` (condition + Warning Event) when a bootstrap connect
/// found **no** repository at the backend and `spec.create.enabled` is `false`, so
/// kopiur declined to initialize one. Distinct from a kopia error class so the
/// "just needs `create.enabled: true`" case is never conflated with a real backend
/// `NotFound` ([`crate::io::BootstrapFailure`]).
pub const REPOSITORY_NOT_INITIALIZED_REASON: &str = "RepositoryNotInitialized";
/// `action` (remediation hint) for [`REPOSITORY_NOT_INITIALIZED_REASON`]: enable
/// repository creation (or point at an existing repository).
pub const ENABLE_CREATE_ACTION: &str = "EnableRepositoryCreate";

/// Condition type for the opt-in backend health probe (`spec.health.probe`).
/// `True` = the last probe reached the backend and the kopia repository is
/// present; `False` with reason [`REPOSITORY_VANISHED_REASON`] or
/// [`BACKEND_UNREACHABLE_REASON`] = the debounced probe found a problem.
/// NON-BLOCKING: the repository stays `Ready` and backups/replication keep
/// running — this is an *alert*, never an outage gate (mirrors
/// [`INDEX_BLOB_HEALTH_CONDITION`]). Wire-visible.
pub const BACKEND_REACHABLE_CONDITION: &str = "BackendReachable";
/// `reason` on [`BACKEND_REACHABLE_CONDITION`] when the probe succeeded.
pub const BACKEND_REACHABLE_REASON: &str = "Reachable";
/// `reason` (condition + Warning event) when the probe found the backend
/// **reachable but the kopia repository absent** (format blob gone) for a
/// repository that was previously `Ready` — a candidate *vanished* repository.
/// Distinct from [`REPOSITORY_NOT_INITIALIZED_REASON`] precisely so the
/// dangerous "set spec.create.enabled: true" advice is NEVER shown for a wipe.
pub const REPOSITORY_VANISHED_REASON: &str = "RepositoryVanished";
/// `reason` when the probe could not confirm an empty repository — the backend is
/// unreachable, the mount/path is missing, or credentials/lock failed. NOT a
/// wipe; kopiur never acts on it.
pub const BACKEND_UNREACHABLE_REASON: &str = "BackendUnreachable";
/// `action` for [`REPOSITORY_VANISHED_REASON`]: a human must verify the backend is
/// truly empty (data blobs may still remain) before any deliberate re-create.
/// kopiur deliberately does NOT auto-recreate.
pub const VERIFY_BACKEND_ACTION: &str = "VerifyBackendBeforeRecreate";

// Every reconcile error is surfaced as a Warning Event on the failing object
// (via `error_policy_for` → `io::reconcile_failure_event`), so a failure is
// visible in `kubectl get events`/`describe` for **every** CRD kind, not only
// the ones with bespoke in-reconcile publishes. A kopia failure reuses the
// kopia class as its `reason` (see `backend_failure_event`); the non-kopia
// `Error` variants get the reasons/actions below:

/// Event `reason` when a reconcile failed on a Kubernetes API call.
pub const KUBE_API_ERROR_REASON: &str = "KubeApiError";
/// `action` for a failed Kubernetes API call: check API-server health and the
/// controller's RBAC.
pub const CHECK_API_SERVER_ACTION: &str = "CheckApiServer";
/// Event `reason` when defensive re-validation rejected the object's spec.
pub const INVALID_SPEC_REASON: &str = "InvalidSpec";
/// `action` for a spec that failed validation: the user must fix the spec.
pub const FIX_SPEC_ACTION: &str = "FixSpec";
/// Event `reason` when a referenced object (Repository, SnapshotPolicy, …) was
/// not found.
pub const MISSING_DEPENDENCY_REASON: &str = "MissingDependency";
/// `action` for a missing dependency: create it or fix the reference.
pub const CHECK_REFERENCES_ACTION: &str = "CheckReferences";
/// Event `reason` when JSON (de)serialization of a spec/status failed.
pub const SERIALIZATION_FAILED_REASON: &str = "SerializationFailed";
/// `action` for failures that indicate a kopiur bug (serialization, violated
/// invariants): report the issue.
pub const REPORT_ISSUE_ACTION: &str = "ReportIssue";
/// Event `reason` when a cron expression failed to parse at scheduling time.
pub const INVALID_SCHEDULE_REASON: &str = "InvalidSchedule";
/// `action` for an unparseable cron expression: fix the schedule in the spec.
pub const FIX_SCHEDULE_ACTION: &str = "FixSchedule";
/// Event `reason` when an object lacked a field the reconciler requires.
pub const INVARIANT_VIOLATED_REASON: &str = "InvariantViolated";
/// Event `reason` when a reconcile is blocked on an out-of-band grant an admin
/// applies on ANOTHER object (e.g. the `privileged-movers` namespace annotation).
pub const BLOCKED_ON_GRANT_REASON: &str = "BlockedOnGrant";
/// `action` for a blocked grant: apply the named grant on the named object —
/// the granting object is watched, so the blocked CR re-reconciles the moment
/// the grant lands.
pub const APPLY_GRANT_ACTION: &str = "ApplyGrant";
/// Event `reason` when self-managed webhook TLS setup failed.
pub const WEBHOOK_SETUP_FAILED_REASON: &str = "WebhookSetupFailed";
/// `action` for a webhook TLS setup failure: check the webhook configuration.
pub const CHECK_WEBHOOK_CONFIGURATION_ACTION: &str = "CheckWebhookConfiguration";

/// Annotation the controller stamps on the self-managed webhook TLS Secret
/// recording the serving leaf's `notAfter` as a Unix timestamp (seconds). Read
/// back to decide leaf rotation without parsing the certificate
/// ([`crate::webhook_tls`]).
pub const WEBHOOK_CERT_NOT_AFTER_ANNOTATION: &str =
    "kopiur.home-operations.com/webhook-cert-not-after";

// --- kopia web-UI server (spec.server) -------------------------------------

/// Finalizer on a `ClusterRepository` whose server children (Deployment/Service/
/// Secret) live in a *different* namespace than the (cluster-scoped) CR. A
/// cluster-scoped owner cannot own a namespaced object via ownerReferences, so GC
/// cannot reap them — this finalizer drives explicit, label-targeted cleanup.
pub const SERVER_CLEANUP_FINALIZER: &str = "kopiur.home-operations.com/server-cleanup";

/// Label identifying an object managed as a kopia server child (value: the kopia
/// server component name). Used as the `Service` selector and the cleanup selector.
pub const SERVER_COMPONENT_LABEL: &str = "app.kubernetes.io/component";
/// Value of [`SERVER_COMPONENT_LABEL`] for kopia server objects.
pub const SERVER_COMPONENT_VALUE: &str = "kopia-server";
/// Label back-referencing the owning (cluster-scoped) `ClusterRepository` by name,
/// so `.watches()` can map a child event to its parent without an ownerReference.
pub const CLUSTER_REPOSITORY_LABEL: &str = "kopiur.home-operations.com/cluster-repository";
/// Companion to [`CLUSTER_REPOSITORY_LABEL`]: the parent UID, guarding against a
/// delete/recreate name collision.
pub const CLUSTER_REPOSITORY_UID_LABEL: &str = "kopiur.home-operations.com/cluster-repository-uid";
/// `app.kubernetes.io/instance` label (the owning repository name).
pub const SERVER_INSTANCE_LABEL: &str = "app.kubernetes.io/instance";
/// `app.kubernetes.io/name` label for server objects.
pub const SERVER_NAME_LABEL: &str = "app.kubernetes.io/name";
/// `app.kubernetes.io/name` value for server objects.
pub const SERVER_NAME_VALUE: &str = "kopiur-server";
