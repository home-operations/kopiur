//! Controller-internal string constants: event reasons/actions, single-flight
//! labels, deadlines (ADR §4.5).
//!
//! The *wire-contract* strings (finalizer, origin/config/dedup labels,
//! `managed-by`, kstatus condition types, API version) live in
//! [`kopiur_api::consts`] so external tooling shares one definition; they are
//! re-exported here so controller call sites keep their existing import paths.
//!
//! The **structural gates** — the condition/reason pairs a reconciler stamps
//! when work is blocked on something only a human can change — are likewise
//! `kopiur_api::consts` strings, enumerated as typed rows in
//! [`kopiur_api::gates::STRUCTURAL_GATES`] so the `kubectl kopiur doctor` side
//! of the contract can never fall behind the controller side (#359). Their
//! Event `action` hints ([`ALLOW_PRIVILEGED_MOVER_ACTION`],
//! [`APPLY_GRANT_ACTION`], [`ACKNOWLEDGE_MASS_DELETION_ACTION`]) stay
//! controller-side: they are remediation copy, not wire contract.

pub use kopiur_api::consts::{
    ALLOW_MASS_DELETION_ANNOTATION, API_VERSION, BLOCKED_ON_UNREADABLE_RUN_REASON, CONFIG_LABEL,
    CREDENTIALS_AVAILABLE_CONDITION, DELETION_HELD_CONDITION, FANOUT_TOO_LARGE_REASON,
    INDEX_BLOB_HEALTH_CONDITION, MAINTENANCE_CONFIGURED_CONDITION, MANAGED_BY_LABEL,
    MANAGED_BY_VALUE, MASS_DELETION_BREAKER_REASON, MASS_DELETION_HELD_CONDITION,
    MASS_DELETION_THRESHOLD_EXCEEDED_REASON, MISSING_CA_BUNDLE_REASON, MISSING_CREDENTIALS_REASON,
    MISSING_SERVICE_ACCOUNT_REASON, MOVER_PERMITTED_CONDITION, OP_LABEL, OP_RESTORE,
    OP_RESTORE_TARGET, ORIGIN_LABEL, PRIVILEGED_MOVER_NOT_PERMITTED_REASON,
    PRIVILEGED_MOVERS_ANNOTATION, READY_CONDITION, RECONCILING_CONDITION,
    REPOSITORIES_READY_CONDITION, REPOSITORY_NOT_READY_REASON, REPOSITORY_READ_ONLY_REASON,
    REPOSITORY_UID_LABEL, REPOSITORY_WRITABLE_CONDITION, RUN_MODE_ANNOTATION,
    RUN_REQUESTED_ANNOTATION, SCHEDULE_FANOUT_CAPPED_CONDITION, SCHEDULE_LABEL,
    SCHEDULE_RUNNABLE_CONDITION, SKIP_SNAPSHOT_CLEANUP_ANNOTATION, SNAPSHOT_CLEANUP_FINALIZER,
    SNAPSHOT_ID_LABEL, SOURCE_PVC_AVAILABLE_CONDITION, SOURCE_PVC_MISSING_REASON,
    STALLED_CONDITION,
};

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

/// Default deadline for a backup parked on a missing DIRECT source PVC
/// ([`SOURCE_PVC_AVAILABLE_CONDITION`] = `False`): how long a `Snapshot` is
/// held `Pending` (re-probed on the structural 300s cadence) from the gate
/// condition's `lastTransitionTime` before it transitions to terminal
/// `Failed`. Bounded so a `concurrencyPolicy: Forbid` schedule is released and
/// `failedJobsHistoryLimit` can prune the row — each new schedule slot then
/// re-probes the PVC live (CronJob semantics). Overridable via
/// [`crate::config::SOURCE_PVC_DEADLINE_ENV`] (`0` = park indefinitely).
pub const DEFAULT_SOURCE_PVC_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(30 * 60);
/// `reason` for [`SOURCE_PVC_AVAILABLE_CONDITION`] = `True`: the source PVC
/// resolved on a later launch attempt (the clear-side counterpart of
/// [`SOURCE_PVC_MISSING_REASON`]; controller-internal remediation copy, like
/// [`FANOUT_WITHIN_CAP_REASON`]). Written ONLY when the condition already
/// exists — the healthy wire never grows the condition.
pub const SOURCE_PVC_FOUND_REASON: &str = "SourcePvcFound";
/// Event `action` (remediation hint) for [`SOURCE_PVC_MISSING_REASON`]:
/// recreate the named PVC, or update the policy's `spec.sources`.
pub const RECREATE_SOURCE_PVC_ACTION: &str = "RecreateSourcePvcOrUpdateSources";

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
/// Label tying a verification Job to the ONE repository it verifies (#368
/// multi-repo fan-out). Value: the stable 6-hex repo tag
/// ([`crate::naming::repo_tag6`]) over the normalized repo key — label-safe
/// where the raw `Kind/ns/name` key (slashes) is not. Scopes the single-flight
/// selector per (policy, repository), so a multi-repo policy's N per-repo
/// verify Jobs run concurrently while each repository still gets at most one.
/// Stamped on every verify Job; single-repo single-flight deliberately keeps
/// selecting on [`VERIFY_INSTANCE_LABEL`] alone so in-flight Jobs from an
/// older operator (which lack this label) still hold the gate across an
/// upgrade.
pub const VERIFY_REPO_LABEL: &str = "kopiur.home-operations.com/verify-repo";

/// `COMPONENT_LABEL` value for replication mover Jobs (ADR-0005 §13(d)).
pub const REPLICATION_COMPONENT: &str = "replication";
/// Label tying a replication Job back to its owning `RepositoryReplication`.
pub const REPLICATION_INSTANCE_LABEL: &str = "kopiur.home-operations.com/replication";
/// Annotation on a replication Job recording the scheduled slot it runs (RFC3339).
pub const REPLICATION_SLOT_ANNOTATION: &str = "kopiur.home-operations.com/replication-slot";

/// Annotation on a bootstrap Job recording the `Repository` generation it was
/// launched FOR. The terminal-Job recycle gate compares it against the live
/// generation: a spec edit makes any lingering terminal Job stale and recycles
/// it immediately — phase-independent, so a user fixing a terminally-`Failed`
/// repository's config re-bootstraps at once instead of waiting out the failed
/// Job's TTL. Comparing the JOB's stamp (not `status.observedGeneration`) is
/// what makes this livelock-free: the freshly-launched Job carries the current
/// generation, so its own result is always consumed, never recycled.
pub const BOOTSTRAP_GENERATION_ANNOTATION: &str = "kopiur.home-operations.com/bootstrap-generation";

/// `COMPONENT_LABEL` value for snapshot-replication (logical, `kopia snapshot
/// migrate`) mover Jobs (issue #368).
pub const SNAPSHOT_REPLICATION_COMPONENT: &str = "snapshot-replication";
/// Label tying a snapshot-replication Job back to its owning
/// `SnapshotReplication` (single-flight selector: [`COMPONENT_LABEL`] +
/// this = CR name). Deliberately the SAME label the replication mover stamps on
/// copy `Snapshot` CRs ([`kopiur_api::consts::SNAPSHOT_REPLICATION_LABEL`]) —
/// one label, one meaning: "belongs to this SnapshotReplication". The
/// component label disambiguates Jobs from copy CRs where it matters.
pub const SNAPSHOT_REPLICATION_INSTANCE_LABEL: &str =
    kopiur_api::consts::SNAPSHOT_REPLICATION_LABEL;
/// Annotation on a snapshot-replication Job recording the scheduled slot it
/// runs (RFC3339; not a valid *label* value because of the colons).
pub const SNAPSHOT_REPLICATION_SLOT_ANNOTATION: &str =
    "kopiur.home-operations.com/snapshot-replication-slot";

/// Annotation on the kopia-server Deployment's POD TEMPLATE carrying a short
/// hash of the serialized `ServerWorkSpec`. The server reads its spec from a
/// mounted ConfigMap, and a ConfigMap content change alone never restarts a
/// running pod — so any server-spec change (a rotated `tls.caBundleRef` CA, a
/// new port/auth mode) must move this annotation to roll the Deployment.
pub const SERVER_SPEC_HASH_ANNOTATION: &str = "kopiur.home-operations.com/server-spec-hash";

/// Annotation a `Snapshot` stamps (RFC3339) on its repository when a backup fails,
/// requesting an immediate connectivity re-probe. Honored once via
/// `status.lastReverifyAt`; rate-limited so a wave of failures forces one re-probe.
pub const REVERIFY_REQUESTED_ANNOTATION: &str = "kopiur.home-operations.com/reverify-requested-at";

/// Annotation stamped on a `Repository`/`ClusterRepository` (writer: the policy
/// reconciler, M6) to REQUEST an on-demand catalog scan — e.g. after adopting a
/// delete-then-recreated repository, so its discovered snapshots materialize
/// immediately instead of waiting for the next spec change or (opt-in) periodic
/// refresh. The RFC3339 timestamp VALUE is an opaque token: honored once via
/// `status.catalog.scanRequestHonored` (equality, never a `lastRefreshAt`
/// comparison — see [`crate::catalog::scan_requested_due`]), and rate-limited via
/// `status.catalog.scanRequestAttemptAt` so a pending token against an
/// unreachable backend cannot recreate bootstrap Jobs on every reconcile.
pub const CATALOG_SCAN_REQUESTED_ANNOTATION: &str =
    "kopiur.home-operations.com/catalog-scan-requested-at";

/// Normal Event reason on a `SnapshotPolicy` when auto-adoption re-attached one
/// or more discovered snapshots into it (M6). The note names the count, the
/// resolved identity, that the rows are now GFS-governed, and both opt-outs.
pub const SNAPSHOTS_ADOPTED_REASON: &str = "SnapshotsAdopted";
/// Event `action` (guidance) for [`SNAPSHOTS_ADOPTED_REASON`]: how to opt out of
/// automatic adoption.
pub const REVIEW_ADOPTION_ACTION: &str = "ReviewAdoption";
/// Normal Event reason on a `SnapshotPolicy` when it stamped an on-demand
/// catalog-scan request ([`CATALOG_SCAN_REQUESTED_ANNOTATION`]) on its
/// repository so newly-recreated snapshots materialize for adoption (M6).
pub const ADOPTION_SCAN_REQUESTED_REASON: &str = "AdoptionScanRequested";
/// Event `action` (guidance) for [`ADOPTION_SCAN_REQUESTED_REASON`].
pub const AWAIT_CATALOG_SCAN_ACTION: &str = "AwaitCatalogScan";
/// Normal Event reason on a `SnapshotPolicy` when the retention-aware adoption
/// gate (adoption inv. 8) left identity-matching discovered snapshots
/// unadopted because `spec.retention` would immediately prune them under an
/// effective `deletionPolicy` of `Retain`/`Orphan`. Transition-gated: published
/// when the withheld count changes, not every pass.
pub const ADOPTION_SKIPPED_BY_RETENTION_REASON: &str = "AdoptionSkippedByRetention";
/// Event `action` (guidance) for [`ADOPTION_SKIPPED_BY_RETENTION_REASON`]: the
/// levers that change the outcome (retention width, deletionPolicy, pin, opt-out).
pub const REVIEW_RETENTION_ACTION: &str = "ReviewRetention";

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
/// `Restore` Failed reason (and Warning Event) when the claiming PVC was bound out from
/// under a populate that was still RUNNING — a provisioner handed it a volume without
/// honoring its `dataSourceRef`, so the restore can never be delivered. Distinct from
/// [`RESTORE_TARGET_ALREADY_BOUND_REASON`]: that one is a benign no-op over a claim that was
/// already carrying its data; this one means the app is about to start on the WRONG volume.
pub const POPULATE_HIJACKED_REASON: &str = "PopulateHijacked";
/// `Restore` Ready reason while a populator that had completed as an already-bound no-op
/// re-resolves its source, because its claiming PVC was re-created (and is unbound again,
/// so it genuinely wants populating). Also re-anchors the `waitTimeout` window.
pub const RESTORE_CLAIM_RECREATED_REASON: &str = "ClaimRecreated";
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
/// Annotation stamped with the RFC 3339 instant of the projection that wrote a
/// credential copy. It has two jobs, and both are load-bearing for the sweep:
///
/// 1. **It makes the re-projection a real write.** A stable copy is refreshed in
///    place by a force server-side apply, and an SSA that yields a byte-identical
///    object is a no-op at the apiserver — no write, and crucially *no
///    `resourceVersion` bump*. Every sweep delete is pinned to the
///    `resourceVersion` the sweep classified, so without a moving field a run that
///    re-projects between the sweep's LIST and its DELETE would NOT invalidate the
///    precondition, and the sweep would delete a Secret a live Job is about to read
///    (`CreateContainerConfigError`). A fresh timestamp per projection guarantees
///    the RV moves, so that delete 409s and the copy is spared.
/// 2. **It is the copy's real age.** `creationTimestamp` never resets on an object
///    refreshed in place, so it cannot answer "has any run touched this lately?".
///    This can. Absent (every copy written before this annotation existed), callers
///    fall back to `creationTimestamp`.
pub const PROJECTED_AT_ANNOTATION: &str = "kopiur.home-operations.com/projected-at";

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

/// Condition reason: waiting for the shared `VolumeGroupSnapshot` to become
/// `readyToUse` (`groupBy: VolumeGroupSnapshot`).
pub const GROUP_STAGING_WAITING_REASON: &str = "WaitingForVolumeGroupSnapshot";
/// Condition reason: the shared `VolumeGroupSnapshot` was still reporting an
/// error when the staging deadline passed. Never produced on first sight of an
/// error — snapshot-controller errors are transient until proven persistent.
pub const REASON_VGS_FAILED: &str = "VolumeGroupSnapshotFailed";
/// Condition reason: the shared `VolumeGroupSnapshot` did not become
/// `readyToUse` within the staging deadline and reported no error.
pub const REASON_GROUP_STAGING_TIMEOUT: &str = "GroupStagingTimedOut";
/// Condition reason: the group became `readyToUse` but captured no member for
/// THIS Snapshot's PVC — the PVC's labels most likely stopped matching the
/// selector between expansion and capture (the snapshot-controller re-evaluates
/// the selector at create time), or the driver excluded it from the group.
pub const REASON_GROUP_MEMBER_MISSING: &str = "GroupMemberMissing";
/// Condition reason: no usable `VolumeGroupSnapshotClass` for the source driver.
pub const REASON_NO_GROUP_CLASS: &str = "NoVolumeGroupSnapshotClass";
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
/// Condition reporting what `mover.inheritSecurityContextFrom` actually achieved this run.
/// Reported ONLY when inheritance did something other than plainly work, so it is a
/// standing "look here" signal rather than routine noise:
///
/// - [`INHERIT_FALLBACK_REASON`] — no workload pod resolved; the recipe's explicit context
///   stood in for it.
/// - [`INHERIT_PINNED_NO_UID_REASON`] — a pod resolved but pins no identity to copy, so
///   inheriting was a no-op and the mover runs as its own image's UID.
/// - [`INHERIT_OVERRIDDEN_REASON`] — an explicit `runAsUser` displaced the inherited one, so
///   inheritance is not tracking the workload for that field.
///
/// On a `Restore` with `inheritSecurityContextFrom.snapshot` (recorded-identity inherit)
/// the SAME condition reports positively too — provable provenance is that mode's value:
///
/// - [`RECORDED_APPLIED_REASON`] — `True`; the backup's recorded identity was applied
///   (message names the Snapshot, the recorded uid, and the provenance).
/// - [`RECORDED_PINNED_NO_UID_REASON`] — `False`; the record pins nothing beyond the
///   hardened baseline, so inheriting it was a provable no-op.
/// - [`MISSING_RECORDED_IDENTITY_REASON`] — `False`; no recorded identity is resolvable
///   yet (missing CR / no `status.recorded` / catalog scan pending) — a slow-requeued hold.
///
/// Distinct from [`SECURITY_CONTEXT_COMPATIBLE_CONDITION`], which is about whether the mover
/// can *read the source*; this is about whether *inheritance did what you asked*.
pub const SECURITY_CONTEXT_INHERITED_CONDITION: &str = "SecurityContextInherited";
/// `reason` for [`SECURITY_CONTEXT_INHERITED_CONDITION`] = `False` when inheritance could not
/// resolve a workload pod and the recipe's explicit context stood in for it.
pub const INHERIT_FALLBACK_REASON: &str = "InheritFallback";
/// `reason` for [`SECURITY_CONTEXT_INHERITED_CONDITION`] = `False` when the resolved workload
/// pins no `runAsUser`/`runAsGroup`/`fsGroup`/`supplementalGroups` — its identity comes from
/// its image, which is invisible in the pod spec, so inheriting copied nothing usable.
pub const INHERIT_PINNED_NO_UID_REASON: &str = "InheritPinnedNoUid";
/// `reason` for [`SECURITY_CONTEXT_INHERITED_CONDITION`] = `False` when an explicit
/// `mover.securityContext.runAsUser` displaced the UID inheritance resolved. Explicit wins by
/// design; this exists so that choice can never be a *silent* no-op — without it a recipe
/// written as a "fallback" would pin the mover forever with no signal when the workload moves.
pub const INHERIT_OVERRIDDEN_REASON: &str = "InheritOverridden";
/// `reason` for [`SECURITY_CONTEXT_INHERITED_CONDITION`] = `True`: inheritance resolved a
/// workload and its values survived every higher merge layer.
///
/// The healthy state is a real, written verdict rather than "write nothing", so that a stale
/// `False` from a previous reconcile is CLEARED once the user fixes the recipe. Conditions are
/// sticky: an arm that returns without writing leaves the old warning contradicting reality
/// forever (the reason [`MOVER_PERMITTED_CONDITION`] needs an explicit clear-the-stale-False
/// block).
pub const INHERIT_APPLIED_REASON: &str = "InheritApplied";
/// Event `action` (remediation hint) for the inherit-pinned-no-uid case.
pub const PIN_WORKLOAD_RUN_AS_USER_ACTION: &str = "PinWorkloadRunAsUser";
/// `reason` for [`SECURITY_CONTEXT_INHERITED_CONDITION`] = `False` on a `Restore` whose
/// `inheritSecurityContextFrom.snapshot` cannot resolve a recorded identity yet: the
/// referenced `Snapshot` CR is missing, carries no `status.recorded` (pre-feature or
/// foreign backup), or no CR matching the source identity exists (catalog scan pending).
/// Permanent-shaped: the hold requeues on the slow structural cadence, and clears when the
/// scan lands/backfills or the spec pins an explicit context.
pub const MISSING_RECORDED_IDENTITY_REASON: &str = "MissingRecordedIdentity";
/// `reason` for [`SECURITY_CONTEXT_INHERITED_CONDITION`] = `False` on a `Restore` when the
/// backup's recorded metadata pins nothing beyond the mover's own hardened baseline (no
/// uid, no gid, fsGroup absent or the hardened 65532) — the restore counterpart of
/// [`INHERIT_PINNED_NO_UID_REASON`]: inheriting the record is a provable no-op and the
/// mover runs as its image's uid.
pub const RECORDED_PINNED_NO_UID_REASON: &str = "RecordedPinnedNoUid";
/// `reason` for [`SECURITY_CONTEXT_INHERITED_CONDITION`] = `True` on a `Restore`: the
/// identity recorded on the backup (`Snapshot.status.recorded`) was applied to the restore
/// mover. The message always names the Snapshot, the recorded uid, and the provenance
/// (`src`) — only `src: inherited` may claim the identity tracked the workload, and a
/// recorded ROOT uid is named explicitly so a forged tag stays auditable (§3.4).
pub const RECORDED_APPLIED_REASON: &str = "RecordedApplied";
/// Event `action` (remediation hint) when recorded-identity inherit cannot proceed or
/// contributed nothing: pin `mover.securityContext` explicitly (or drop the inherit).
pub const SET_EXPLICIT_MOVER_CONTEXT_ACTION: &str = "SetExplicitMoverSecurityContext";
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

// `SCHEDULE_FANOUT_CAPPED_CONDITION` / `FANOUT_TOO_LARGE_REASON` moved to
// `kopiur_api::consts` (re-exported above): the M10 gates/doctor checklist
// promoted the fan-out cap into `kopiur_api::gates::SCHEDULE_FANOUT_CAPPED_GATE`,
// so both sides of the doctor contract read one row. The clear-side reason stays
// controller-internal (remediation copy, like `Settled`).
/// `reason` for [`SCHEDULE_FANOUT_CAPPED_CONDITION`] = `False`.
pub const FANOUT_WITHIN_CAP_REASON: &str = "FanoutWithinCap";

/// Machine-readable `reason` (condition + Warning Event) when a bootstrap connect
/// found **no** repository at the backend and `spec.create.enabled` is `false`, so
/// kopiur declined to initialize one. Distinct from a kopia error class so the
/// "just needs `create.enabled: true`" case is never conflated with a real backend
/// `NotFound` ([`crate::io::BootstrapFailure`]).
pub const REPOSITORY_NOT_INITIALIZED_REASON: &str = "RepositoryNotInitialized";
/// `action` (remediation hint) for [`REPOSITORY_NOT_INITIALIZED_REASON`]: enable
/// repository creation (or point at an existing repository).
pub const ENABLE_CREATE_ACTION: &str = "EnableRepositoryCreate";

/// `action` for the source-side `spec.seed` failures (issue #380): the seed
/// source is not usable — a mis-pointed bucket/prefix, or a mirror that was
/// never written. The seeding bootstrap keeps retrying, so the fix is to point
/// `spec.seed.from` somewhere real (or set `allowEmptySource`).
pub const CHECK_SEED_SOURCE_ACTION: &str = "CheckSeedSource";
/// `action` for the seed failures kopiur RESUMES by itself (an interrupted copy:
/// `SeedIncomplete`/`SeedLeftEmpty`, issue #380). Nothing at the backend should
/// be deleted — the next bootstrap continues the copy — so the only human action
/// is to give the copy room to finish (raise
/// `spec.seed.failurePolicy.activeDeadlineSeconds`) if it keeps being cut short.
pub const AWAIT_SEED_RESUME_ACTION: &str = "AwaitSeedResume";

// The mover-skew guard's `reason` lives in `kopiur_api::consts` beside the other
// `Seeded` reasons, not here: `kopiur_api::gates` registers it, and the
// phase-ratchet rule keeps a gating condition's strings in the api crate.
/// `action` for [`kopiur_api::consts::SEED_MOVER_TOO_OLD_REASON`]: upgrade the
/// mover image.
pub const UPGRADE_MOVER_IMAGE_ACTION: &str = "UpgradeMoverImage";

/// `action` for [`kopiur_mover::bootstrap::BOOTSTRAP_INTERNAL_INCONSISTENCY_CLASS`]:
/// the mover disagreed with itself. Not a repository problem and not retryable —
/// the same inputs reproduce the same contradiction — so the remediation is to
/// report it.
pub const REPORT_KOPIUR_BUG_ACTION: &str = "ReportKopiurBug";

/// `reason` for the `RepositorySeeded` Normal Event published when a
/// `spec.seed` actually copied this repository's initial contents in (#380).
pub const REPOSITORY_SEEDED_REASON: &str = "RepositorySeeded";
/// `action` for [`REPOSITORY_SEEDED_REASON`]: seeded history is adopted by
/// re-applying `SnapshotPolicy`s, and GFS retention prunes beyond-budget points
/// immediately — so review retention/`defaultDeletionPolicy` before doing that.
pub const REVIEW_SEEDED_HISTORY_ACTION: &str = "ReviewSeededHistory";

/// `reason` for the Warning Event published when a `SnapshotPolicy`'s adoption
/// pass finds a NON-EMPTY repository catalog but ZERO snapshots matching its
/// identity (#380). The identity fork guards are Update-gated and cannot fire on
/// a fresh-cluster CREATE, so after a disaster recovery a policy whose identity
/// config differs even slightly from the pre-disaster one adopts nothing while
/// looking perfectly healthy.
pub const NO_ADOPTABLE_HISTORY_REASON: &str = "NoAdoptableHistory";
/// `action` for [`NO_ADOPTABLE_HISTORY_REASON`]: the recovered identity must
/// match the pre-disaster one byte for byte.
pub const CHECK_IDENTITY_ACTION: &str = "CheckIdentityConfiguration";

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

/// Finalizer on a `SnapshotPolicy`, driving the deletion cascade onto its
/// `Snapshot` children (`spec.deletion.onPolicyDelete`,
/// [`crate::snapshot_policy::plan_policy_cascade`]) before the CR is removed.
/// Controller-only — the webhook never stamps it.
pub const POLICY_CLEANUP_FINALIZER: &str = "kopiur.home-operations.com/policy-cleanup";

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

// --- Mass-deletion protection (ADR: DeletionProtectionSpec) ----------------

/// [`OP_LABEL`] value for the mass-deletion batch-delete mover Job — several
/// `Snapshot`s' kopia manifests deleted in one Job (M2's `SnapshotDeleteBatch`
/// mover op). Mirrors the shape of [`OP_RESTORE_POPULATE`]; the existing
/// single-delete finalizer Job stamps its own value as a string literal today
/// rather than through a named const.
pub const OP_SNAPSHOT_DELETE_BATCH: &str = "snapshot-delete-batch";
/// Annotation on a batch-delete Job listing the comma-joined, SORTED
/// `Snapshot` UIDs it targets — the dispatcher's single source of truth for
/// "which Snapshots does this Job cover" (single-flight / no-overlap checks).
/// An annotation, not a label: an arbitrary-length UID list can exceed a
/// label-value's 63-char limit for anything but a tiny batch.
pub const DELETE_MEMBERS_ANNOTATION: &str = "kopiur.home-operations.com/delete-members";
/// Label on a batch-delete Job carrying the target repository's hash
/// (`crate::snapshot::repo_label`), so the dispatcher can LIST live batch Jobs
/// for one repository (single-flight / concurrency-cap enforcement) without
/// listing every batch Job in the install scope.
pub const DELETE_REPO_LABEL: &str = "kopiur.home-operations.com/delete-repo";

/// Event reason on a `Snapshot` whose own deletion is held by the
/// mass-deletion breaker.
pub const SNAPSHOT_DELETION_HELD_REASON: &str = "SnapshotDeletionHeld";
/// Event reason when a `Snapshot` is retained by the schedule-deletion cascade
/// guard (`onScheduleDelete: Retain` with a gone/replaced owning
/// `SnapshotSchedule`).
pub const SNAPSHOT_RETAINED_ON_SCHEDULE_DELETE_REASON: &str = "SnapshotRetainedOnScheduleDelete";
/// Event reason when a `Snapshot` is retained by the policy-deletion cascade
/// (`pruned-by: policy-cascade`, stamped when the owning `SnapshotPolicy` is
/// gone and `onPolicyDelete: Retain`).
pub const SNAPSHOT_RETAINED_ON_POLICY_DELETE_REASON: &str = "SnapshotRetainedOnPolicyDelete";
/// Event reason when [`ALLOW_MASS_DELETION_ANNOTATION`]'s value fails to parse
/// as an RFC3339 timestamp — the ack is ignored (fail-safe) rather than
/// silently disarming the breaker.
pub const INVALID_MASS_DELETION_ACK_REASON: &str = "InvalidMassDeletionAck";

/// `reason` for [`MASS_DELETION_HELD_CONDITION`] = `False`: pending external
/// destructive deletions are below the breaker threshold (or the breaker is off).
pub const MASS_DELETION_BELOW_THRESHOLD_REASON: &str = "BelowThreshold";
/// `reason` stamped on a `Snapshot`'s [`DELETION_HELD_CONDITION`] = `False` when a
/// previously-held deletion proceeds because the wave was acknowledged (or the
/// breaker fell back below threshold).
pub const MASS_DELETION_ACKNOWLEDGED_REASON: &str = "Acknowledged";

/// Event `action` (remediation hint) for the mass-deletion breaker's Warning
/// events (`SnapshotDeletionHeld`, `MassDeletionHeld`, `InvalidMassDeletionAck`):
/// approve the wave via the `allow-mass-deletion` annotation.
pub const ACKNOWLEDGE_MASS_DELETION_ACTION: &str = "AcknowledgeMassDeletion";
/// Event `action` for `SnapshotRetainedOnScheduleDelete`: opt into cascading
/// deletes by setting the schedule's `spec.deletion.onScheduleDelete: Delete`.
pub const ENABLE_SCHEDULE_CASCADE_ACTION: &str = "EnableScheduleCascade";
/// Event `action` for `SnapshotRetainedOnPolicyDelete`: opt into cascading
/// deletes by setting the policy's `spec.deletion.onPolicyDelete: Delete`.
pub const ENABLE_POLICY_CASCADE_ACTION: &str = "EnablePolicyCascade";
