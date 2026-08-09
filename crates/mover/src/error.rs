//! The mover's typed error surface (ADR §5.5, [[actionable-error-messages]]).
//!
//! One `thiserror` enum per failure domain instead of stringly `anyhow`
//! wrapping: the structured [`kopiur_kopia::KopiaError`] (class, stderr tail,
//! exit code) survives **all the way to the status PATCH** — stringification
//! happens only at the [`FailureBlock`](crate::status::FailureBlock) /
//! log-line surface, never before. Every kopia call site names which
//! invocation failed via [`KopiaOp`], so a `kubectl logs` line or a
//! `status.failure.message` always says *what* failed, not just *that* kopia
//! exited non-zero.

use std::path::PathBuf;

use kopiur_kopia::{KopiaError, KopiaErrorClass};

/// Which kopia invocation a [`MoverError::Kopia`] failure came from. Stable,
/// human-greppable labels for messages and logs; exhaustive — a new mover flow
/// must name its operations here before it can fail (ADR §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KopiaOp {
    /// `repository connect` on the generic backup/restore/delete/pin path.
    RepositoryConnect,
    /// `repository throttle set` (moverDefaults.throttle).
    ThrottleSet,
    /// `policy set` against the snapshot's source identity.
    PolicySet,
    /// `snapshot create`.
    SnapshotCreate,
    /// `snapshot restore`.
    SnapshotRestore,
    /// `snapshot delete` (the Snapshot finalizer path).
    SnapshotDelete,
    /// `snapshot pin`/`unpin` reconciliation.
    SnapshotPin,
    /// `repository connect` for a maintenance run.
    MaintenanceConnect,
    /// `maintenance info` (lease holder read).
    MaintenanceInfo,
    /// `maintenance set-owner` (kopia's per-connection owner guard).
    MaintenanceSetOwner,
    /// `maintenance run`.
    MaintenanceRun,
    /// `repository connect` for a verification run.
    VerifyConnect,
    /// `snapshot verify` (quick tier).
    SnapshotVerify,
    /// `snapshot list` while resolving an object-store restore source (the in-Job
    /// `fromPolicy`/`identity` listing path).
    RestoreSnapshotList,
    /// `snapshot list` while resolving the deep-verify restore candidate.
    DeepVerifySnapshotList,
    /// The deep-verify scratch restore.
    DeepVerifyRestore,
    /// `repository connect` to the replication *source*.
    ReplicateConnect,
    /// `repository sync-to` the replication destination.
    RepositorySyncTo,
    /// `repository connect --readonly` for a browse session.
    BrowseConnect,
    /// `repository connect --readonly --persist-credentials` to the snapshot
    /// replication SOURCE (under the dedicated `srepl-source` config).
    SnapshotReplicateSourceConnect,
    /// `repository connect` to the snapshot replication DESTINATION (under the
    /// dedicated `srepl-dest` config, read-write).
    SnapshotReplicateDestConnect,
    /// The persisted-password probe: `repository status` on the source config
    /// with `KOPIA_PASSWORD` REMOVED. Must succeed — it proves the persisted
    /// password alone opens the source, which is what `snapshot migrate`'s
    /// source open reads FIRST (env wins only for normal opens).
    SourcePasswordProbe,
    /// `snapshot migrate --source-config` into the destination.
    SnapshotMigrate,
    /// `snapshot list --all` on the replication SOURCE.
    SourceSnapshotList,
    /// `snapshot list --all` on the replication DESTINATION.
    DestSnapshotList,
}

impl KopiaOp {
    /// The stable label used in messages/logs (matches the historical
    /// `"<op> failed (class …)"` strings, so logs stay greppable).
    pub fn as_str(&self) -> &'static str {
        match self {
            KopiaOp::RepositoryConnect => "repository connect",
            KopiaOp::ThrottleSet => "repository throttle set",
            KopiaOp::PolicySet => "policy set",
            KopiaOp::SnapshotCreate => "snapshot create",
            KopiaOp::SnapshotRestore => "snapshot restore",
            KopiaOp::SnapshotDelete => "snapshot delete",
            KopiaOp::SnapshotPin => "snapshot pin",
            KopiaOp::MaintenanceConnect => "maintenance connect",
            KopiaOp::MaintenanceInfo => "maintenance info",
            KopiaOp::MaintenanceSetOwner => "maintenance set-owner",
            KopiaOp::MaintenanceRun => "maintenance run",
            KopiaOp::VerifyConnect => "verify connect",
            KopiaOp::SnapshotVerify => "snapshot verify",
            KopiaOp::RestoreSnapshotList => "restore snapshot list",
            KopiaOp::DeepVerifySnapshotList => "deep verify snapshot list",
            KopiaOp::DeepVerifyRestore => "deep verify restore",
            KopiaOp::ReplicateConnect => "replication connect",
            KopiaOp::RepositorySyncTo => "repository sync-to",
            KopiaOp::BrowseConnect => "browse session connect",
            KopiaOp::SnapshotReplicateSourceConnect => "snapshot replication source connect",
            KopiaOp::SnapshotReplicateDestConnect => "snapshot replication destination connect",
            KopiaOp::SourcePasswordProbe => "source password probe",
            KopiaOp::SnapshotMigrate => "snapshot migrate",
            KopiaOp::SourceSnapshotList => "source snapshot list",
            KopiaOp::DestSnapshotList => "destination snapshot list",
        }
    }
}

/// How many missing `(identity, startTime)` pairs a
/// [`MoverError::MigrateIncomplete`] message lists before truncating — the
/// status message must stay readable (and under the apiserver's limits) even
/// when a whole first-full-history run failed.
pub const MISSING_SAMPLE_CAP: usize = 10;

/// Everything the mover binary can fail on. Replaces the old `anyhow` paths so
/// the typed cause (and the kopia class behind it) is preserved until the
/// status PATCH / process exit.
#[derive(Debug, thiserror::Error)]
pub enum MoverError {
    /// A kopia subprocess call failed. Names the invocation and keeps the full
    /// [`KopiaError`] (class, stderr tail, exit code) as the source.
    #[error("{} failed (class {}): {}", .op.as_str(), .source.class(), .source)]
    Kopia {
        /// Which kopia invocation failed.
        op: KopiaOp,
        /// The structured kopia failure.
        #[source]
        source: KopiaError,
    },

    /// A repository bootstrap ended unsuccessfully; the class/message are read
    /// back from the persisted [`BootstrapResult`](crate::bootstrap::BootstrapResult)
    /// failure block (the class arrives as its stable label).
    #[error("repository bootstrap failed (class {class}): {message}")]
    BootstrapFailed {
        /// The kopia error class the bootstrap recorded.
        class: KopiaErrorClass,
        /// The bootstrap's persisted failure message.
        message: String,
    },

    /// No work spec was provided at all.
    #[error(
        "no work spec: pass a path as the first arg, or set {} (inline JSON, how the \
         controller passes it) or {} (a file path)",
        crate::env::WORK_SPEC,
        crate::env::WORK_SPEC_PATH
    )]
    WorkSpecPathMissing,

    /// The work-spec file could not be read.
    #[error(
        "failed to read the work spec at {}: {source} — check the path (for a \
         controller-created Job the spec is inline in the {} env instead)",
        .path.display(),
        crate::env::WORK_SPEC
    )]
    WorkSpecRead {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// The work-spec file is not valid `MoverWorkSpec` JSON.
    #[error(
        "failed to parse the work spec at {}: {source}. The controller and mover image versions \
         may be skewed — redeploy so both run the same kopiur version",
        .path.display()
    )]
    WorkSpecParse {
        /// The path holding the malformed spec.
        path: PathBuf,
        /// The underlying JSON error.
        #[source]
        source: serde_json::Error,
    },

    /// No server work-spec path was provided to the `serve` entrypoint.
    #[error(
        "no server spec path: pass it after `serve` or set {}",
        crate::env::SERVER_SPEC_PATH
    )]
    ServerSpecPathMissing,

    /// The server work-spec file could not be read.
    #[error(
        "failed to read the server spec at {}: {source}. The controller mounts it via the \
         server work-spec ConfigMap — check the Deployment's volume mount and {}",
        .path.display(),
        crate::env::SERVER_SPEC_PATH
    )]
    ServerSpecRead {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// The server work-spec file is not valid `ServerWorkSpec` JSON.
    #[error(
        "failed to parse the server spec at {}: {source}. The controller and mover image versions \
         may be skewed — redeploy so both run the same kopiur version",
        .path.display()
    )]
    ServerSpecParse {
        /// The path holding the malformed spec.
        path: PathBuf,
        /// The underlying JSON error.
        #[source]
        source: serde_json::Error,
    },

    /// The credential staging directory could not be created.
    #[error(
        "failed to create the credential staging dir {}: {source}. The kopia-cache emptyDir must \
         be mounted and writable by the mover's UID",
        .path.display()
    )]
    CredentialStagingDir {
        /// The staging directory.
        path: PathBuf,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// A file-based backend credential (SFTP key, GCS JSON, rclone.conf) could
    /// not be written from its environment variable.
    #[error(
        "failed to write the credential file {} (from ${env_key}): {source}. Check the \
         credentials Secret key and that the kopia-cache emptyDir is writable",
        .path.display()
    )]
    CredentialWrite {
        /// The env var the credential came from.
        env_key: &'static str,
        /// The destination file.
        path: PathBuf,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// The browse-session readiness marker could not be written, so the
    /// session pod would never turn Ready and the CLI would hang waiting.
    #[error(
        "failed to write the browse-session readiness marker {}: {source}. The kopia-cache \
         emptyDir must be mounted at /var/cache/kopia and writable by the mover's UID",
        .path.display()
    )]
    ReadyMarkerWrite {
        /// The marker path ([`crate::env::READY_MARKER`]).
        path: PathBuf,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// Deep verification found no snapshot to scratch-restore.
    #[error(
        "deep verify found no snapshot to restore for source path {source_path:?}: the \
         repository has no snapshot for this identity yet. Run a backup first (the operator \
         normally schedules verification only after the first successful backup)"
    )]
    VerifyNoSnapshot {
        /// The identity source path the lookup keyed on.
        source_path: String,
    },

    /// An object-store restore selector matched NO snapshot once the wait window
    /// closed and `onMissingSnapshot: Fail` was in effect. Terminal (a Failed
    /// Restore never retries); the fix is to create the snapshot, widen
    /// `source.asOf`/`offset`, or choose `Continue` to come up empty.
    #[error(
        "no snapshot matched the restore source ({identity}) within the wait window; create the \
         snapshot, widen source.asOf/offset, or set onMissingSnapshot: Continue to come up empty"
    )]
    RestoreNoSnapshot {
        /// The kopia identity the listing keyed on (`user@host:path`).
        identity: String,
    },

    /// A restore selector's `asOf` was not a valid RFC3339 timestamp. The webhook
    /// validates this at admission, so this is the defensive in-Job path.
    #[error("restore source asOf {as_of:?} is not an RFC3339 timestamp: {message}")]
    RestoreAsOfInvalid {
        /// The offending value.
        as_of: String,
        /// The parse error text.
        message: String,
    },

    /// The deep-verify scratch path is not writable by the mover, so the
    /// scratch-restore would fail with a cryptic kopia `mkdir` error. Caught by a
    /// preflight probe before kopia runs (the non-root mover cannot create a dir
    /// under root-owned `/` unless a writable volume is mounted at the path).
    #[error(
        "deep verify scratch path {} is not writable by the mover (uid {uid}): {source}. The \
         controller must mount a writable volume there — set verification.deep.capacity (or \
         moverDefaults.scratch.capacity), optionally with a storageClassName, to provision a \
         sized ephemeral PVC, or leave them unset for an emptyDir",
        .path.display()
    )]
    ScratchNotWritable {
        /// The scratch path that could not be written ([`crate::jobs::DEEP_SCRATCH_PATH`]).
        path: PathBuf,
        /// The mover's UID (for the message; the hardened non-root default unless overridden).
        uid: i64,
        /// The underlying IO error from the writability probe.
        #[source]
        source: std::io::Error,
    },

    /// The user's verification `successExpr` evaluated to `false`.
    #[error("verification successExpr evaluated false: {expr:?}")]
    SuccessExprFalse {
        /// The CEL expression that rejected the run.
        expr: String,
    },

    /// The verification `successExpr` could not be evaluated at all.
    #[error("verification successExpr failed to evaluate: {source}")]
    SuccessExprEval {
        /// The evaluation error (bad expression / non-bool result).
        #[source]
        source: kopiur_api::ValidationError,
    },

    /// A kube client could not be built (the side-channel status PATCHes need
    /// in-cluster ServiceAccount credentials).
    #[error(
        "failed to build a kube client: {source}. In-cluster ServiceAccount credentials are \
         required for status PATCHes"
    )]
    KubeClient {
        /// The underlying kube error (boxed: `kube::Error` is large and this
        /// enum rides in every mover `Result`).
        #[source]
        source: Box<kube::Error>,
    },

    /// A CR status PATCH failed.
    #[error("failed to PATCH the status of {kind} {namespace}/{name}: {source}")]
    StatusPatch {
        /// The target CR kind.
        kind: String,
        /// The target CR namespace.
        namespace: String,
        /// The target CR name.
        name: String,
        /// The underlying kube error (boxed, see [`MoverError::KubeClient`]).
        #[source]
        source: Box<kube::Error>,
    },

    /// The bootstrap result could not be serialized.
    #[error("failed to serialize the bootstrap result: {source}")]
    ResultSerialize {
        /// The underlying JSON error.
        #[source]
        source: serde_json::Error,
    },

    /// The bootstrap result could not be written into the work-spec ConfigMap.
    #[error(
        "failed to write the bootstrap result into ConfigMap {namespace}/{configmap}: {source}. \
         The controller cannot read the outcome — check the mover Role's ConfigMap patch \
         permission"
    )]
    ResultConfigMapPatch {
        /// The ConfigMap name.
        configmap: String,
        /// The ConfigMap namespace.
        namespace: String,
        /// The underlying kube error (boxed, see [`MoverError::KubeClient`]).
        #[source]
        source: Box<kube::Error>,
    },

    /// The replication destination's kopia password env var is unset. The
    /// controller injects it via a `secretKeyRef` under the dedicated name so
    /// it can never collide with the source's `KOPIA_PASSWORD`.
    #[error(
        "the replication destination's kopia password is missing: ${env_key} is unset. The \
         controller injects it from the destination repository's encryption Secret — check the \
         mover Job's env and that the destination Secret (or its projected copy) exists"
    )]
    DestPasswordMissing {
        /// The env var that should carry the destination password
        /// ([`crate::env::DEST_KOPIA_PASSWORD`]).
        env_key: &'static str,
    },

    /// `kopia snapshot migrate` exited 0 but the post-verify listing found
    /// expected `(identity, startTime)` pairs missing on the destination.
    /// kopia's per-source migration goroutines only LOG their errors, so exit
    /// code 0 does not mean every selected snapshot arrived — the post-verify
    /// is the real success gate.
    #[error(
        "snapshot replication is incomplete: {missing} of {expected} expected snapshot(s) did \
         not arrive on the destination after `kopia snapshot migrate` (which exits 0 even when \
         a per-source migration fails — see the mover pod logs for kopia's per-source errors). \
         Missing (up to {sample_cap} shown): {sample}. The run will be retried; migrate is \
         idempotent by (identity, startTime), so a retry only copies what is still missing",
        sample_cap = MISSING_SAMPLE_CAP
    )]
    MigrateIncomplete {
        /// How many expected snapshots are missing on the destination.
        missing: usize,
        /// How many snapshots were expected in total.
        expected: usize,
        /// A capped, human-readable `identity@startTime` sample list.
        sample: String,
    },

    /// Some dest-side copy `Snapshot` CRs could not be created/stamped/deduped
    /// this run. The reconciliation is SSA-idempotent and re-runs over the full
    /// correspondence set every run, so a retry converges.
    #[error(
        "snapshot replication copied data but {failed} of {total} destination-side copy \
         Snapshot CR(s) could not be reconciled; see the mover pod logs for the per-CR kube \
         errors. The reconciliation is idempotent (server-side apply over the full \
         correspondence set), so the next run re-attempts only what is still missing"
    )]
    CopyCrSyncIncomplete {
        /// How many copy-CR reconciliations failed.
        failed: usize,
        /// The correspondence set's size.
        total: usize,
    },

    /// A copy-CR LIST (the reconciliation's or pruning's candidate read)
    /// failed, so the whole wave could not even start.
    #[error(
        "snapshot replication could not list Snapshot CRs for {context}: {source}. Check the \
         dedicated snapshot-replication mover Role (get/list/create/patch/delete on snapshots \
         + snapshots/status patch) and the apiserver's availability; the run will be retried"
    )]
    ReplicationCrList {
        /// Which wave needed the LIST ("copy-CR reconciliation" / "pruning").
        context: &'static str,
        /// The underlying kube error (boxed, see [`MoverError::KubeClient`]).
        #[source]
        source: Box<kube::Error>,
    },

    /// Some prune deletes (retention/mirrorSource) failed this run. Pruning
    /// re-selects from live state every run, so a retry converges.
    #[error(
        "snapshot replication pruning completed incompletely: {failed} of {total} copy \
         Snapshot CR delete(s) failed; see the mover pod logs for the per-CR kube errors. \
         Pruning re-selects from live state each run, so the next run re-attempts what remains"
    )]
    PruneIncomplete {
        /// How many prune deletes failed.
        failed: usize,
        /// How many prune deletes were attempted.
        total: usize,
    },

    /// Telemetry init failed under `KOPIUR_OTEL_STRICT` (without strict mode it
    /// degrades inside `init_tracing` and never reaches here).
    #[error(transparent)]
    Telemetry(#[from] kopiur_telemetry::TelemetryError),

    /// A `SnapshotDeleteBatch` run deleted some, but not all, of its members.
    /// Every member is attempted independently — never short-circuited by an
    /// earlier failure, since kopia's delete is idempotent and every retry of
    /// the WHOLE batch monotonically shrinks the truly-remaining set — so a
    /// transient repo blip mid-batch converges on the next Job retry instead
    /// of wedging on the first failure. Per-item causes are only logged (a
    /// `warn!` per failing member); this variant just names how many failed,
    /// since the batch has no per-item CR status to carry a breakdown.
    #[error(
        "batch snapshot delete completed incompletely: {failed} of {total} member deletes \
         failed; see the mover pod logs for the per-item kopia errors. Deletes are idempotent, \
         so retrying (the Job, or the next scheduled batch) only re-attempts what is still \
         outstanding"
    )]
    BatchDeleteIncomplete {
        /// How many of the batch's members failed to delete.
        failed: usize,
        /// The batch's total member count.
        total: usize,
    },
}

impl MoverError {
    /// The kopia error class this failure maps to — what
    /// [`FailureBlock`](crate::status::FailureBlock) persists and the
    /// controller keys retry decisions on. Exhaustive `match`, no `_ =>`
    /// (ADR §5.5): a new variant cannot compile until it is classified.
    ///
    /// Kopia/bootstrap failures delegate to the real class. Everything else is
    /// environmental/config — re-running the same pod will not help — so it
    /// maps to [`KopiaErrorClass::Unknown`] (non-retryable), matching how
    /// [`KopiaError::Spawn`] is treated.
    pub fn kopia_class(&self) -> KopiaErrorClass {
        match self {
            MoverError::Kopia { source, .. } => source.class(),
            MoverError::BootstrapFailed { class, .. } => *class,
            MoverError::WorkSpecPathMissing
            | MoverError::WorkSpecRead { .. }
            | MoverError::WorkSpecParse { .. }
            | MoverError::ServerSpecPathMissing
            | MoverError::ServerSpecRead { .. }
            | MoverError::ServerSpecParse { .. }
            | MoverError::CredentialStagingDir { .. }
            | MoverError::CredentialWrite { .. }
            | MoverError::ReadyMarkerWrite { .. }
            | MoverError::VerifyNoSnapshot { .. }
            | MoverError::RestoreNoSnapshot { .. }
            | MoverError::RestoreAsOfInvalid { .. }
            | MoverError::ScratchNotWritable { .. }
            | MoverError::SuccessExprFalse { .. }
            | MoverError::SuccessExprEval { .. }
            | MoverError::KubeClient { .. }
            | MoverError::StatusPatch { .. }
            | MoverError::ResultSerialize { .. }
            | MoverError::ResultConfigMapPatch { .. }
            | MoverError::Telemetry(_)
            | MoverError::BatchDeleteIncomplete { .. }
            | MoverError::DestPasswordMissing { .. }
            | MoverError::MigrateIncomplete { .. }
            | MoverError::CopyCrSyncIncomplete { .. }
            | MoverError::ReplicationCrList { .. }
            | MoverError::PruneIncomplete { .. } => KopiaErrorClass::Unknown,
        }
    }

    /// Whether the operator should retry the same operation (delegates to the
    /// class's own hint).
    pub fn retry_recommended(&self) -> bool {
        self.kopia_class().is_retryable()
    }
}

/// Result alias for mover code.
pub type Result<T, E = MoverError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kopia_op_labels_are_stable_for_every_variant() {
        // Loop over every op: the label must be non-empty, lowercase-stable,
        // and distinct (these strings are greppable log/message anchors).
        let all = [
            KopiaOp::RepositoryConnect,
            KopiaOp::ThrottleSet,
            KopiaOp::PolicySet,
            KopiaOp::SnapshotCreate,
            KopiaOp::SnapshotRestore,
            KopiaOp::SnapshotDelete,
            KopiaOp::SnapshotPin,
            KopiaOp::MaintenanceConnect,
            KopiaOp::MaintenanceInfo,
            KopiaOp::MaintenanceSetOwner,
            KopiaOp::MaintenanceRun,
            KopiaOp::VerifyConnect,
            KopiaOp::SnapshotVerify,
            KopiaOp::RestoreSnapshotList,
            KopiaOp::DeepVerifySnapshotList,
            KopiaOp::DeepVerifyRestore,
            KopiaOp::ReplicateConnect,
            KopiaOp::RepositorySyncTo,
            KopiaOp::BrowseConnect,
            KopiaOp::SnapshotReplicateSourceConnect,
            KopiaOp::SnapshotReplicateDestConnect,
            KopiaOp::SourcePasswordProbe,
            KopiaOp::SnapshotMigrate,
            KopiaOp::SourceSnapshotList,
            KopiaOp::DestSnapshotList,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for op in all {
            let label = op.as_str();
            assert!(!label.is_empty());
            assert!(seen.insert(label), "duplicate op label {label}");
        }
        // Pin the EXACT connect label: it is persisted into
        // `status.failure.op` and the controller's repository-shaped nudge
        // gate compares against it (#345) — silent drift would also orphan the
        // op values already written into existing CR statuses.
        assert_eq!(KopiaOp::RepositoryConnect.as_str(), "repository connect");
    }

    #[test]
    fn kopia_class_delegates_to_the_source_class() {
        // A retryable kopia failure (locked repo) stays retryable through the
        // mover wrapper; an auth failure stays terminal.
        let locked = MoverError::Kopia {
            op: KopiaOp::MaintenanceRun,
            source: KopiaError::NonZeroExit {
                args: "maintenance run".into(),
                code: Some(1),
                class: KopiaErrorClass::Locked,
                stderr_tail: "repository is locked".into(),
            },
        };
        assert_eq!(locked.kopia_class(), KopiaErrorClass::Locked);
        assert!(locked.retry_recommended());

        let auth = MoverError::BootstrapFailed {
            class: KopiaErrorClass::AuthFailure,
            message: "invalid repository password".into(),
        };
        assert_eq!(auth.kopia_class(), KopiaErrorClass::AuthFailure);
        assert!(!auth.retry_recommended());
    }

    #[test]
    fn environmental_failures_classify_unknown_and_non_retryable() {
        // Config/environment problems don't fix themselves on a blind re-run.
        let parse = MoverError::WorkSpecParse {
            path: PathBuf::from("/spec/work.json"),
            source: serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
        };
        assert_eq!(parse.kopia_class(), KopiaErrorClass::Unknown);
        assert!(!parse.retry_recommended());

        let cred = MoverError::CredentialWrite {
            env_key: "KOPIA_SFTP_KEY_DATA",
            path: PathBuf::from("/kopia-cache/creds/sftp_key"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        assert_eq!(cred.kopia_class(), KopiaErrorClass::Unknown);
    }

    // --- message texts a human acts on (the what/why/fix rule) ---

    #[test]
    fn kopia_message_preserves_the_historical_op_failed_shape() {
        let err = MoverError::Kopia {
            op: KopiaOp::MaintenanceConnect,
            source: KopiaError::NonZeroExit {
                args: "repository connect".into(),
                code: Some(1),
                class: KopiaErrorClass::RepositoryUnavailable,
                stderr_tail: "dial tcp: connection refused".into(),
            },
        };
        let msg = err.to_string();
        assert!(
            msg.starts_with("maintenance connect failed (class RepositoryUnavailable):"),
            "{msg}"
        );
        assert!(msg.contains("connection refused"), "{msg}");
    }

    #[test]
    fn bootstrap_failed_message_is_byte_identical_to_the_historical_string() {
        // The controller and e2e logs grep for this exact shape; it must not
        // drift when the anyhow! call became a typed variant.
        let err = MoverError::BootstrapFailed {
            class: KopiaErrorClass::AccessDenied,
            message: "Access Denied".into(),
        };
        assert_eq!(
            err.to_string(),
            "repository bootstrap failed (class AccessDenied): Access Denied"
        );
    }

    #[test]
    fn work_spec_messages_name_the_env_var_and_path() {
        assert!(
            MoverError::WorkSpecPathMissing
                .to_string()
                .contains("KOPIUR_WORK_SPEC_PATH")
        );
        let read = MoverError::WorkSpecRead {
            path: PathBuf::from("/spec/work.json"),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        let msg = read.to_string();
        assert!(msg.contains("/spec/work.json"), "{msg}");
        // The fix points at the inline-env contract (the controller no longer
        // mounts a work-spec ConfigMap).
        assert!(msg.contains("KOPIUR_WORK_SPEC"), "{msg}");
    }

    #[test]
    fn credential_write_names_the_env_key_and_the_fix() {
        let err = MoverError::CredentialWrite {
            env_key: "KOPIA_GCS_CREDENTIALS",
            path: PathBuf::from("/kopia-cache/creds/gcs-credentials.json"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let msg = err.to_string();
        assert!(msg.contains("$KOPIA_GCS_CREDENTIALS"), "{msg}");
        assert!(msg.contains("credentials Secret"), "{msg}");
        assert!(msg.contains("emptyDir is writable"), "{msg}");
    }

    #[test]
    fn ready_marker_write_names_the_path_and_the_fix() {
        let err = MoverError::ReadyMarkerWrite {
            path: PathBuf::from(crate::env::READY_MARKER),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("/var/cache/kopia/.kopiur-session-ready"),
            "{msg}"
        );
        assert!(msg.contains("writable by the mover's UID"), "{msg}");
        assert_eq!(err.kopia_class(), KopiaErrorClass::Unknown);
    }

    #[test]
    fn scratch_not_writable_names_the_path_uid_and_fix_and_is_non_retryable() {
        let err = MoverError::ScratchNotWritable {
            path: PathBuf::from("/scratch"),
            uid: 65532,
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let msg = err.to_string();
        // what: the path and the running uid
        assert!(msg.contains("/scratch"), "{msg}");
        assert!(msg.contains("uid 65532"), "{msg}");
        // fix: the actionable knobs the user/operator sets
        assert!(msg.contains("verification.deep.capacity"), "{msg}");
        assert!(msg.contains("emptyDir"), "{msg}");
        // an environmental/config problem: a blind re-run won't help
        assert_eq!(err.kopia_class(), KopiaErrorClass::Unknown);
        assert!(!err.retry_recommended());
    }

    #[test]
    fn verify_no_snapshot_message_is_what_why_fix_without_the_nonexistent_field() {
        let err = MoverError::VerifyNoSnapshot {
            source_path: "/pvc/agregarr".into(),
        };
        let msg = err.to_string();
        // what: no snapshot found for the source path
        assert!(msg.contains("/pvc/agregarr"), "{msg}");
        assert!(msg.contains("no snapshot to restore"), "{msg}");
        // why: the repository has no snapshot for this identity yet
        assert!(msg.contains("no snapshot for this identity yet"), "{msg}");
        // fix: run a backup first
        assert!(msg.contains("Run a backup first"), "{msg}");
        // must NOT recommend the nonexistent CRD field
        assert!(
            !msg.contains("snapshotID") && !msg.contains("snapshotId"),
            "message must not reference the nonexistent verify.deep.snapshotID field: {msg}"
        );
    }

    #[test]
    fn success_expr_messages_match_the_historical_patch_bodies() {
        let f = MoverError::SuccessExprFalse {
            expr: "stats.files > 0".into(),
        };
        assert_eq!(
            f.to_string(),
            "verification successExpr evaluated false: \"stats.files > 0\""
        );
    }

    #[test]
    fn batch_delete_incomplete_names_the_counts_and_is_idempotent_retry_friendly() {
        let err = MoverError::BatchDeleteIncomplete {
            failed: 2,
            total: 5,
        };
        let msg = err.to_string();
        // what: how many of the batch failed
        assert!(msg.contains("2 of 5 member deletes failed"), "{msg}");
        // fix: retrying only re-attempts what's outstanding (idempotent deletes)
        assert!(msg.contains("idempotent"), "{msg}");
        assert_eq!(err.kopia_class(), KopiaErrorClass::Unknown);
    }

    #[test]
    fn dest_password_missing_names_the_env_var_and_the_fix() {
        let err = MoverError::DestPasswordMissing {
            env_key: crate::env::DEST_KOPIA_PASSWORD,
        };
        let msg = err.to_string();
        // what: the exact env var
        assert!(msg.contains("$KOPIUR_DEST_KOPIA_PASSWORD"), "{msg}");
        // fix: where the value comes from
        assert!(msg.contains("encryption Secret"), "{msg}");
        assert_eq!(err.kopia_class(), KopiaErrorClass::Unknown);
        assert!(!err.retry_recommended());
    }

    #[test]
    fn migrate_incomplete_names_counts_sample_and_the_exit_zero_caveat() {
        let err = MoverError::MigrateIncomplete {
            missing: 3,
            expected: 12,
            sample: "a@h:/p@2026-08-01T00:00:00Z, b@h:/q@2026-08-02T00:00:00Z".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("3 of 12"), "{msg}");
        // why: kopia exits 0 on per-source failures — the caveat that makes
        // the post-verify mandatory must be stated to the reader.
        assert!(msg.contains("exits 0"), "{msg}");
        // sample list is present, retry guidance names the idempotency key.
        assert!(msg.contains("a@h:/p@2026-08-01T00:00:00Z"), "{msg}");
        assert!(msg.contains("(identity, startTime)"), "{msg}");
        assert_eq!(err.kopia_class(), KopiaErrorClass::Unknown);
    }

    #[test]
    fn copy_cr_and_prune_incompletes_name_counts_and_convergence() {
        let sync = MoverError::CopyCrSyncIncomplete {
            failed: 2,
            total: 40,
        };
        let msg = sync.to_string();
        assert!(msg.contains("2 of 40"), "{msg}");
        assert!(msg.contains("idempotent"), "{msg}");

        let prune = MoverError::PruneIncomplete {
            failed: 1,
            total: 5,
        };
        let msg = prune.to_string();
        assert!(msg.contains("1 of 5"), "{msg}");
        assert!(msg.contains("re-selects from live state"), "{msg}");
    }

    #[test]
    fn source_chain_is_preserved() {
        let err = MoverError::Kopia {
            op: KopiaOp::SnapshotCreate,
            source: KopiaError::EmptyOutput {
                context: "snapshot create".into(),
                stderr_tail: String::new(),
            },
        };
        assert!(
            std::error::Error::source(&err).is_some(),
            "the KopiaError source must stay inspectable"
        );
    }
}
