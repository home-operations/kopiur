//! Status types the mover PATCHes onto the `Snapshot`/`Restore` `.status`
//! subresource, plus the **pure** mapping from kopia results/errors to those
//! types.
//!
//! The pure mapping (`KopiaError → FailureBlock`, `SnapshotCreateResult →
//! `kopiur_api::SnapshotStats`/`SnapshotTiming`) is unit-testable with no cluster.
//! The stats/timing types are the CRD's own (not mover-local) so their field
//! names cannot drift from the structural schema — a mismatch is silently pruned
//! by the API server. The actual kube PATCH lives in a thin function gated so
//! tests don't need a client.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use kopiur_api::common::ResolvedIdentity;
use kopiur_api::restore::{ResolutionOutcome, ResolvedRestore, RestorePhase};
use kopiur_api::snapshot::SnapshotInfo;
use kopiur_api::{PhaseLabel, SnapshotStats, SnapshotTiming};
use kopiur_kopia::{KopiaError, MaintenanceMode, SnapshotCreateResult};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::error::{MoverError, Result};
use crate::workspec::{self, MaintenanceOp, MoverWorkSpec};

/// Map a kopia create result to the CRD `status.stats` shape (`SnapshotStats`).
///
/// We reuse the API type rather than a mover-local struct so the field names
/// stay in lockstep with the `Snapshot` CRD's structural schema. They MUST match:
/// the API server **prunes unknown status fields**, so a drifting name (the old
/// mover-local `totalBytes`/`fileCount`) is silently dropped and `status.stats`
/// lands as `{}` — which is exactly the bug that left `kopiur_snapshot_size_bytes`
/// empty. kopia's snapshot-create summary reports the snapshot's total size and
/// file count, mapped to `sizeBytes`/`filesNew`.
fn stats_from_result(r: &SnapshotCreateResult) -> SnapshotStats {
    // Entries kopia couldn't read and EXCLUDED (only non-empty on the exit-0
    // ignore-file-errors path; the fatal exit-1 path never reaches a success status).
    let failed = r.entry_errors().len();
    SnapshotStats {
        size_bytes: Some(r.total_bytes() as i64),
        files_new: Some(r.file_count() as i64),
        files_failed: (failed > 0).then_some(failed as i64),
        ..Default::default()
    }
}

/// Map a kopia create result to the CRD `status.snapshot` (`SnapshotInfo`).
///
/// MUST be the nested `{ kopiaSnapshotID, identity }` shape, not a flat
/// `snapshotId`: the API server prunes unknown status fields, so a flat field is
/// silently dropped and `status.snapshot` never lands — which is exactly why
/// object-store backups recorded `Succeeded` with no snapshot id. The identity
/// comes from kopia's recorded source (`user@host:path`), which the controller
/// pinned via `--override-source`.
fn snapshot_from_result(r: &SnapshotCreateResult) -> SnapshotInfo {
    SnapshotInfo {
        kopia_snapshot_id: r.id.clone(),
        identity: ResolvedIdentity {
            username: r.source.user_name.clone(),
            hostname: r.source.host.clone(),
            source_path: Some(r.source.path.clone()),
        },
        // Surface the kopia description (`snapshot create --description`) so a
        // produced run's `status.snapshot.description` reflects what was stored.
        // Empty = none recorded = elided from the PATCH.
        description: (!r.description.is_empty()).then(|| r.description.clone()),
    }
}

/// Map a kopia create result's start/end timestamps to the CRD `status.timing`.
fn timing_from_result(r: &SnapshotCreateResult) -> SnapshotTiming {
    SnapshotTiming {
        start_time: Some(r.start_time.to_rfc3339()),
        end_time: Some(r.end_time.to_rfc3339()),
        duration_seconds: Some((r.end_time - r.start_time).num_seconds()),
    }
}

/// The structured terminal-failure block (ADR §4.10), re-exported from
/// `kopiur-api` so the field names are the CRD's own — the API server prunes a
/// status field the schema doesn't define, which is exactly how the original
/// mover-local `FailureBlock` was silently lost (`SnapshotStatus` had no
/// `failure` property until the API type landed).
pub use kopiur_api::common::FailureBlock;

/// Build a [`FailureBlock`] from a bare kopia error; the class, stderr tail,
/// exit code, and retry hint all carry through. (A free function, not
/// `From<&KopiaError>`: both types are foreign here, so the impl would violate
/// the orphan rule.)
///
/// ```
/// use kopiur_kopia::{KopiaError, KopiaErrorClass};
/// use kopiur_mover::status::failure_block_from_kopia;
///
/// let err = KopiaError::NonZeroExit {
///     args: "repository connect".into(),
///     code: Some(1),
///     class: KopiaErrorClass::AuthFailure,
///     stderr_tail: "invalid repository password".into(),
/// };
/// let fb = failure_block_from_kopia(&err);
/// assert_eq!(fb.kopia_error_class, "AuthFailure");
/// assert_eq!(fb.exit_code, Some(1));
/// assert_eq!(fb.stderr_tail.as_deref(), Some("invalid repository password"));
/// // A wrong password is not worth a blind retry.
/// assert!(!fb.retry_recommended);
/// ```
pub fn failure_block_from_kopia(err: &KopiaError) -> FailureBlock {
    let class = err.class();
    let exit_code = match err {
        KopiaError::NonZeroExit { code, .. } => *code,
        _ => None,
    };
    FailureBlock {
        kopia_error_class: class.as_str().to_string(),
        message: err.to_string(),
        stderr_tail: err.stderr_tail().map(str::to_string),
        exit_code,
        retry_recommended: class.is_retryable(),
        // No `KopiaOp` in scope here — the bare `KopiaError` carries only its
        // raw argv string, not the mover's stable op label. Callers that know
        // the op wrap the error in `MoverError::Kopia` first (see the
        // `From<&MoverError>` impl below, which does populate `op`).
        op: None,
    }
}

impl From<&crate::error::MoverError> for FailureBlock {
    /// Map a typed mover failure to the persisted block. The structured error
    /// is stringified **only here**, at the status surface; a kopia-backed
    /// failure carries its stderr tail and exit code through, and the class +
    /// retry hint always come from [`MoverError::kopia_class`]
    /// (`crate::error::MoverError::kopia_class`) so they cannot drift from the
    /// message.
    fn from(err: &crate::error::MoverError) -> Self {
        use crate::error::MoverError;
        // `op` is the stable label of the kopia invocation that failed
        // ([`KopiaOp::as_str`](crate::error::KopiaOp::as_str)); everything
        // else failed outside a kopia invocation, so there is no op to record.
        let (stderr_tail, exit_code, op) = match err {
            MoverError::Kopia { op, source } => (
                source.stderr_tail().map(str::to_string),
                match source {
                    KopiaError::NonZeroExit { code, .. } => *code,
                    KopiaError::Spawn { .. }
                    | KopiaError::Json { .. }
                    | KopiaError::EmptyOutput { .. }
                    | KopiaError::Timeout { .. } => None,
                },
                Some(op.as_str().to_string()),
            ),
            MoverError::BootstrapFailed { .. }
            | MoverError::WorkSpecPathMissing
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
            | MoverError::StatusRead { .. }
            | MoverError::ResultSerialize { .. }
            | MoverError::ResultConfigMapPatch { .. }
            | MoverError::Telemetry(_)
            | MoverError::BatchDeleteIncomplete { .. }
            | MoverError::DestPasswordMissing { .. }
            | MoverError::SeedPasswordMissing { .. }
            | MoverError::MigrateIncomplete { .. }
            | MoverError::CopyCrSyncIncomplete { .. }
            | MoverError::ReplicationCrList { .. }
            | MoverError::PruneIncomplete { .. } => (None, None, None),
        };
        FailureBlock {
            kopia_error_class: err.kopia_class().as_str().to_string(),
            message: err.to_string(),
            stderr_tail,
            exit_code,
            retry_recommended: err.retry_recommended(),
            op,
        }
    }
}

/// Truncate to the LAST [`MAX_LOG_TAIL_BYTES`] bytes (the newest output is the
/// actionable part), cutting on a `char` boundary and preferring to start at the
/// first whole line after the cut. Pure.
pub fn capped_tail(s: &str) -> String {
    use kopiur_api::common::MAX_LOG_TAIL_BYTES;
    if s.len() <= MAX_LOG_TAIL_BYTES {
        return s.to_string();
    }
    let mut start = s.len() - MAX_LOG_TAIL_BYTES;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    let tail = &s[start..];
    // Prefer starting on a whole line, as long as that doesn't eat most of the tail.
    match tail.find('\n') {
        Some(nl) if nl + 1 < tail.len() && nl < MAX_LOG_TAIL_BYTES / 8 => {
            tail[nl + 1..].to_string()
        }
        _ => tail.to_string(),
    }
}

/// The `logTail` text for a terminal failure: the actionable message plus the
/// kopia stderr tail (when present), capped. Deterministic given the failure —
/// no timestamps — so a re-patch of the same outcome cannot churn status.
fn failure_log_tail(failure: &FailureBlock) -> String {
    match failure.stderr_tail.as_deref() {
        Some(stderr) if !stderr.is_empty() => {
            capped_tail(&format!("{}\n{}", failure.message, stderr))
        }
        _ => capped_tail(&failure.message),
    }
}

/// The phase a mover run reports.
///
/// Only the TERMINAL mover phases live here. There is deliberately no `Running`
/// variant: the periodic heartbeat carries no phase (the controller owns every
/// in-flight phase), so the mover can never emit a `"Running"` the target CR's
/// enum forbids (the Restore 422). See [`StatusUpdate::progress`].
///
/// ```
/// use kopiur_mover::status::MoverPhase;
///
/// assert_eq!(MoverPhase::Succeeded.as_str(), "Succeeded");
/// assert_eq!(MoverPhase::Failed.as_str(), "Failed");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoverPhase {
    /// The operation completed successfully.
    Succeeded,
    /// The operation failed terminally.
    Failed,
    /// A backup ran to completion but kopia wrote no new manifest, because the
    /// source is byte-identical to the previous snapshot
    /// (`files.ignoreIdenticalSnapshots`).
    ///
    /// A success for every liveness purpose — the source was read and hashed,
    /// and it is protected by the previous snapshot — but this run owns no
    /// kopia manifest, so it must never be reported as `Succeeded`: the
    /// controller would then resolve "its" snapshot and find its predecessor's
    /// (#351).
    Unchanged,
}

impl MoverPhase {
    /// Stable string form for the CR status `phase` field.
    pub fn as_str(&self) -> &'static str {
        match self {
            MoverPhase::Succeeded => "Succeeded",
            MoverPhase::Failed => "Failed",
            MoverPhase::Unchanged => "Unchanged",
        }
    }
}

/// A status update the mover PATCHes onto the CR. This is the payload shape;
/// the kube call wraps it under `{ "status": ... }`.
///
/// [`StatusUpdate::as_patch_body`] nests the payload under `status` for a
/// status-subresource merge PATCH:
///
/// ```
/// use chrono::{DateTime, Utc};
/// use kopiur_mover::status::StatusUpdate;
///
/// let observed_at: DateTime<Utc> = "2026-06-01T12:00:00Z".parse().unwrap();
/// // The periodic heartbeat carries NO phase — the controller owns the
/// // in-flight phase, so the mover never asserts one here.
/// let update = StatusUpdate::progress(observed_at);
/// assert_eq!(update.phase, None);
///
/// let body = update.as_patch_body();
/// assert!(body["status"].get("phase").is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusUpdate {
    /// Current phase, set ONLY by the terminal constructors. The periodic
    /// progress heartbeat leaves it `None`: the controller owns every in-flight
    /// phase (Snapshot→`Running`, Restore→`Restoring`, SnapshotDelete→`Deleting`)
    /// and never reads the mover's, so writing a phase here only risks one the
    /// target CR's enum forbids (the Restore `"Running"` 422). Omitted from the
    /// PATCH when `None`, so the heartbeat never collides with the controller's
    /// phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// When this update was produced.
    pub observed_at: DateTime<Utc>,
    /// The snapshot (CRD `status.snapshot`), once known. Nested `SnapshotInfo`
    /// (`{ kopiaSnapshotID, identity }`) so the API server doesn't prune it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotInfo>,
    /// Timing, on success (CRD `status.timing`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<SnapshotTiming>,
    /// Stats, on success (CRD `status.stats`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<SnapshotStats>,
    /// Failure block, on terminal failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureBlock>,
    /// The last lines of the run's output (CRD `status.logTail`), set ONLY by
    /// the terminal constructors — never by `running()` — so it is written once
    /// per terminal transition and cannot churn status. Bounded by
    /// [`kopiur_api::common::MAX_LOG_TAIL_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_tail: Option<String>,
    /// The pinned restore resolution (CRD `status.resolved`), set ONLY when the
    /// mover RESOLVED the source itself (object-store `fromPolicy`/`identity` — the
    /// in-Job listing path). The controller never pins these (it can't list the
    /// backend in-process), so the mover writes the outcome here for provenance.
    /// Disjoint from the controller-written phase/conditions subtree, so the merge
    /// PATCH never collides. Absent for controller-resolved restores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedRestore>,
}

impl StatusUpdate {
    /// A periodic progress heartbeat. Deliberately carries NO `phase`: the
    /// controller owns every in-flight phase and never reads the mover's, so
    /// asserting one here only risks sending a value the target CR's enum forbids
    /// (the original Restore `"Running"` 422). `logTail` stays unset — it is
    /// written once, at the terminal transition (the status-churn rule).
    ///
    /// Note: the CRD has a `status.progress` (`RestoreProgress`) field for live
    /// restore counters, but it is intentionally NOT populated here yet — kopia's
    /// restore progress is CR-delimited, human-rounded text with no exact byte
    /// total, so wiring it faithfully is tracked as a follow-up.
    pub fn progress(observed_at: DateTime<Utc>) -> Self {
        StatusUpdate {
            phase: None,
            observed_at,
            snapshot: None,
            timing: None,
            stats: None,
            failure: None,
            log_tail: None,
            resolved: None,
        }
    }

    /// A successful backup update from a kopia create result. `logTail` carries
    /// the documented `Snapshot created: <id>` line (ADR §3.4).
    pub fn succeeded_backup(result: &SnapshotCreateResult, observed_at: DateTime<Utc>) -> Self {
        StatusUpdate {
            phase: Some(MoverPhase::Succeeded.as_str().to_string()),
            observed_at,
            snapshot: Some(snapshot_from_result(result)),
            timing: Some(timing_from_result(result)),
            stats: Some(stats_from_result(result)),
            failure: None,
            log_tail: Some(format!("Snapshot created: {}", result.id)),
            resolved: None,
        }
    }

    /// A completed backup that produced NO new kopia manifest: nothing changed
    /// since the previous snapshot, so kopia deduped it away.
    ///
    /// Deliberately carries no `snapshot`, no `stats` and no `timing`. There is
    /// no manifest to point at, and inventing one — say, by resolving the
    /// newest snapshot for this identity — would hand this CR its
    /// predecessor's manifest, which that CR owns via a finalizer and will
    /// delete when retention prunes it (#351).
    pub fn unchanged_backup(started_at: DateTime<Utc>, observed_at: DateTime<Utc>) -> Self {
        StatusUpdate {
            phase: Some(MoverPhase::Unchanged.as_str().to_string()),
            observed_at,
            snapshot: None,
            // Timing IS recorded, from the mover's own clock rather than a
            // manifest's. Without an `endTime` the policy's last-backup
            // timestamp would never advance, and `KopiurBackupStale` would page
            // for a source that is simply not changing — the healthy case this
            // whole feature exists to support (#351).
            timing: Some(SnapshotTiming {
                start_time: Some(started_at.to_rfc3339()),
                end_time: Some(observed_at.to_rfc3339()),
                duration_seconds: Some((observed_at - started_at).num_seconds()),
            }),
            // No stats: there is no manifest to measure. The size/duration/file
            // gauges keep describing the last snapshot that actually exists.
            stats: None,
            failure: None,
            log_tail: Some(
                "No files changed since the previous snapshot; no new snapshot was created"
                    .to_string(),
            ),
            resolved: None,
        }
    }

    /// A successful snapshot-delete update (Snapshot finalizer path) with no stats.
    /// The Snapshot CRD's terminal success phase is `Succeeded`.
    pub fn succeeded(observed_at: DateTime<Utc>) -> Self {
        StatusUpdate {
            phase: Some(MoverPhase::Succeeded.as_str().to_string()),
            observed_at,
            snapshot: None,
            timing: None,
            stats: None,
            failure: None,
            log_tail: None,
            resolved: None,
        }
    }

    /// A successful snapshot-pin/unpin update that re-stamps `status.snapshot`
    /// with the snapshot's CURRENT manifest id.
    ///
    /// kopia's `UpdateSnapshot` (what `snapshot pin`/`unpin` call) saves a NEW
    /// manifest and deletes the old id, so the id recorded at create time is
    /// stale the moment a snapshot is pinned. The pin mover re-resolves the live
    /// id and reports it here so the finalizer delete and `snapshotRef` restore
    /// target the live manifest, not a deleted one. Touches ONLY `status.snapshot`
    /// (no `timing`/`stats`) so the Merge PATCH never disturbs create-time fields,
    /// and `status.pinned` stays the controller's to write (disjoint subtrees ⇒
    /// no two-writer churn). `logTail` is deterministic (no churn).
    pub fn succeeded_pin(snapshot: SnapshotInfo, observed_at: DateTime<Utc>) -> Self {
        let id = snapshot.kopia_snapshot_id.clone();
        StatusUpdate {
            phase: Some(MoverPhase::Succeeded.as_str().to_string()),
            observed_at,
            snapshot: Some(snapshot),
            timing: None,
            stats: None,
            failure: None,
            log_tail: Some(format!("Snapshot pin reconciled: {id}")),
            resolved: None,
        }
    }

    /// A successful restore update with no stats. The Restore CRD's terminal
    /// success phase is `Completed` — NOT `Succeeded` (the Snapshot phase). Writing
    /// `Succeeded` here is rejected by the apiserver with a 422 (the enum forbids
    /// it), so the phase string is sourced from [`RestorePhase::Completed`] to
    /// stay locked to the CRD. `snapshot_id` is the kopia snapshot that was
    /// restored, surfaced on `status.logTail`.
    pub fn completed(snapshot_id: &str, observed_at: DateTime<Utc>) -> Self {
        StatusUpdate {
            phase: Some(RestorePhase::Completed.label().to_string()),
            observed_at,
            snapshot: None,
            timing: None,
            stats: None,
            failure: None,
            log_tail: Some(format!("Restore completed: snapshot {snapshot_id}")),
            resolved: None,
        }
    }

    /// A successful restore the MOVER resolved (object-store `fromPolicy`/`identity`
    /// in-Job path): same terminal `Completed` as [`StatusUpdate::completed`], but
    /// it also pins `status.resolved` with the snapshot the selector resolved to,
    /// since the controller couldn't list the backend in-process to pin it.
    pub fn completed_resolved(
        snapshot_id: &str,
        identity: ResolvedIdentity,
        observed_at: DateTime<Utc>,
    ) -> Self {
        StatusUpdate {
            phase: Some(RestorePhase::Completed.label().to_string()),
            observed_at,
            snapshot: None,
            timing: None,
            stats: None,
            failure: None,
            log_tail: Some(format!("Restore completed: snapshot {snapshot_id}")),
            resolved: Some(ResolvedRestore {
                resolution: Some(ResolutionOutcome::Snapshot),
                kopia_snapshot_id: Some(snapshot_id.to_string()),
                identity: Some(identity),
                pinned_at: Some(observed_at.to_rfc3339()),
                ..Default::default()
            }),
        }
    }

    /// A successful deploy-or-restore where the selector matched NO snapshot and
    /// `onMissingSnapshot: Continue` was in effect: the target is left empty and
    /// `status.resolved` pins the `NoSnapshot` outcome so a later-appearing
    /// snapshot can never silently retarget this Restore.
    pub fn completed_empty(observed_at: DateTime<Utc>) -> Self {
        StatusUpdate {
            phase: Some(RestorePhase::Completed.label().to_string()),
            observed_at,
            snapshot: None,
            timing: None,
            stats: None,
            failure: None,
            log_tail: Some(
                "Restore completed: no snapshot matched the source; left the target \
                 empty (deploy-or-restore)"
                    .to_string(),
            ),
            resolved: Some(ResolvedRestore {
                resolution: Some(ResolutionOutcome::NoSnapshot),
                pinned_at: Some(observed_at.to_rfc3339()),
                ..Default::default()
            }),
        }
    }

    /// A terminal-failure update from a kopia error. `logTail` mirrors the
    /// failure's message + stderr tail so `kubectl get -o yaml` shows the
    /// actionable text without digging into the (possibly reaped) Job pod.
    pub fn failed(err: &KopiaError, observed_at: DateTime<Utc>) -> Self {
        let failure = failure_block_from_kopia(err);
        StatusUpdate {
            phase: Some(MoverPhase::Failed.as_str().to_string()),
            observed_at,
            snapshot: None,
            timing: None,
            stats: None,
            log_tail: Some(failure_log_tail(&failure)),
            failure: Some(failure),
            resolved: None,
        }
    }

    /// A terminal-failure update from a typed mover error — same JSON shape as
    /// [`StatusUpdate::failed`], but the message names which operation failed
    /// and non-kopia failures (work spec, credentials, …) are representable.
    pub fn failed_mover(err: &crate::error::MoverError, observed_at: DateTime<Utc>) -> Self {
        let failure = FailureBlock::from(err);
        StatusUpdate {
            phase: Some(MoverPhase::Failed.as_str().to_string()),
            observed_at,
            snapshot: None,
            timing: None,
            stats: None,
            log_tail: Some(failure_log_tail(&failure)),
            failure: Some(failure),
            resolved: None,
        }
    }

    /// Wrap this update as the `{ "status": ... }` merge-patch body kube
    /// expects for a status subresource PATCH.
    pub fn as_patch_body(&self) -> serde_json::Value {
        serde_json::json!({ "status": self })
    }
}

/// `{ "status": ... }` body for a successful verification.
///
/// `repository_key: None` (the classic single-repo flow): stamp the flat
/// `lastVerified` and a `Verified=True` condition — byte-identical to every
/// prior operator.
///
/// `repository_key: Some(key)` (a multi-repository policy's per-repo verify,
/// #368): stamp ONLY `verificationStamps[<key>]`. A JSON merge patch merges
/// *map keys* but replaces *arrays*, so the entry-keyed map is what lets two
/// concurrent per-repo verifies land without clobbering each other — writing
/// the flat field or the `status.verification` Vec from here would lose one
/// repo's result (and the flat field's multi-repo meaning is the controller's
/// MIN across repos, which one mover cannot compute). No condition either:
/// the conditions array is replace-on-merge, so concurrent per-repo writers
/// must not touch it — the controller folds the stamps and owns conditions.
pub fn verify_ok_body(
    tier: &str,
    repository_key: Option<&str>,
    now: &chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    let ts = now.to_rfc3339();
    match repository_key {
        None => serde_json::json!({
            "status": {
                "lastVerified": ts,
                "conditions": [{
                    "type": "Verified",
                    "status": "True",
                    "reason": "VerificationSucceeded",
                    "message": format!("{tier} verification succeeded"),
                    "lastTransitionTime": ts,
                    "observedGeneration": 0,
                }],
            }
        }),
        Some(key) => serde_json::json!({
            "status": {
                "verificationStamps": { key: ts },
            }
        }),
    }
}

/// `{ "status": ... }` body for a failed verification: a `Verified=False` condition.
pub fn verify_failed_body(message: &str) -> serde_json::Value {
    serde_json::json!({
        "status": {
            "conditions": [{
                "type": "Verified",
                "status": "False",
                "reason": "VerificationFailed",
                "message": message,
                "lastTransitionTime": chrono::Utc::now().to_rfc3339(),
                "observedGeneration": 0,
            }],
        }
    })
}

/// `{ "status": ... }` body for a successful replication: stamp `lastReplicated`,
/// the destination backend, phase `Succeeded`, and a `Ready=True` condition.
pub fn replicate_ok_body(dest: &str, now: &chrono::DateTime<chrono::Utc>) -> serde_json::Value {
    let ts = now.to_rfc3339();
    serde_json::json!({
        "status": {
            "phase": "Succeeded",
            "destinationBackend": dest,
            "lastReplicated": ts,
            "conditions": [{
                "type": "Ready",
                "status": "True",
                "reason": "ReplicationSucceeded",
                "message": format!("replicated to {dest}"),
                "lastTransitionTime": ts,
                "observedGeneration": 0,
            }],
        }
    })
}

/// `{ "status": ... }` body for a failed replication: phase `Failed` + a
/// `Ready=False` condition.
pub fn replicate_failed_body(message: &str) -> serde_json::Value {
    serde_json::json!({
        "status": {
            "phase": "Failed",
            "conditions": [{
                "type": "Ready",
                "status": "False",
                "reason": "ReplicationFailed",
                "message": message,
                "lastTransitionTime": chrono::Utc::now().to_rfc3339(),
                "observedGeneration": 0,
            }],
        }
    })
}

/// The per-run counters a snapshot-replication terminal PATCH reports
/// (`SnapshotReplication.status.lastRun`). Field names are pinned by the
/// serde attrs to the CRD's camelCase wire shape — a drifting name is
/// silently pruned by the apiserver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReplicationRunStats {
    /// How many distinct source identities the selection matched.
    pub identities_selected: usize,
    /// How many snapshots newly arrived on the destination this run.
    pub snapshots_copied: usize,
    /// How many expected snapshots were already on the destination.
    pub already_present: usize,
    /// How many expected snapshots did NOT arrive (post-verify misses).
    pub failed: usize,
    /// How many dest-side copy CRs this run's pruning deleted.
    pub pruned: usize,
}

/// `{ "status": ... }` body for a successful snapshot replication: phase
/// `Succeeded`, `lastReplicated`, the `lastRun` counters, and a `Ready=True`
/// condition. `reason`/`message` are caller-supplied so the zero-match run
/// (`NoIdentitiesMatched`) reads differently from a real copy wave.
/// `observedGeneration` is 0 — the controller heals it (and Ready) on its next
/// pass; the mover never knows the CR's generation (two-pass contract).
pub fn snapshot_replicate_ok_body(
    dest: &str,
    now: &chrono::DateTime<chrono::Utc>,
    stats: &SnapshotReplicationRunStats,
    reason: &str,
    message: &str,
) -> serde_json::Value {
    let ts = now.to_rfc3339();
    serde_json::json!({
        "status": {
            "phase": "Succeeded",
            "lastReplicated": ts,
            "lastRun": stats,
            "conditions": [{
                "type": "Ready",
                "status": "True",
                "reason": reason,
                "message": format!("{message} (destination: {dest})"),
                "lastTransitionTime": ts,
                "observedGeneration": 0,
            }],
        }
    })
}

/// `{ "status": ... }` body for a failed snapshot replication: phase `Failed`
/// and a `Ready=False`/`ReplicationFailed` condition carrying the actionable
/// message. Carries `lastRun` counters when the run got far enough to have
/// them (post-verify misses land here with `failed` set).
pub fn snapshot_replicate_failed_body(
    message: &str,
    stats: Option<&SnapshotReplicationRunStats>,
) -> serde_json::Value {
    let mut status = serde_json::json!({
        "phase": "Failed",
        "conditions": [{
            "type": "Ready",
            "status": "False",
            "reason": "ReplicationFailed",
            "message": message,
            "lastTransitionTime": chrono::Utc::now().to_rfc3339(),
            "observedGeneration": 0,
        }],
    });
    if let Some(s) = stats {
        status["lastRun"] = serde_json::to_value(s).expect("stats serialize");
    }
    serde_json::json!({ "status": status })
}

/// `{ "status": ... }` body for a successful maintenance run. A full run also
/// advances the quick clock (full subsumes quick). `lastContentReclaimedBytes`
/// is `0`: `kopia maintenance run` emits no JSON, so the precise figure needs a
/// `maintenance info` delta (tracked separately; the field round-trips).
pub fn maintenance_ran_body(
    op: &MaintenanceOp,
    now: &chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    let ts = now.to_rfc3339();
    let run = serde_json::json!({ "lastRunAt": ts, "lastContentReclaimedBytes": 0 });
    let mut status = serde_json::json!({
        "ownership": { "owner": op.owner, "claimedAt": ts },
        "conditions": [lease_condition_body("True", "LeaseClaimed", "maintenance lease claimed", now)],
    });
    match op.mode {
        MaintenanceMode::Quick => {
            status["quick"] = run;
        }
        MaintenanceMode::Full => {
            status["quick"] = run.clone();
            status["full"] = run;
        }
    }
    serde_json::json!({ "status": status })
}

/// `{ "status": ... }` body when the lease is held by another owner (yield /
/// prompt): record the observed holder and a `LeaseOwned=False` condition.
pub fn lease_blocked_body(owner: &str, reason: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "status": {
            "ownership": { "owner": owner },
            "conditions": [lease_condition_body("False", reason, message, &chrono::Utc::now())],
        }
    })
}

/// `{ "status": ... }` body for a failed kopia maintenance call.
pub fn maintenance_failed_body(e: &KopiaError) -> serde_json::Value {
    maintenance_failure_body(e.class(), &e.to_string())
}

/// [`maintenance_failed_body`] for a maintenance step whose typed cause is a
/// [`MoverError`] rather than a bare [`KopiaError`] — the repository throttle
/// applied right after the maintenance connect. Same condition shape; the class
/// comes from the error's own classification
/// ([`MoverError::kopia_class`](crate::error::MoverError::kopia_class)) so the
/// controller's retry hint cannot drift from the message.
pub fn maintenance_failed_body_from_mover(e: &MoverError) -> serde_json::Value {
    maintenance_failure_body(e.kopia_class(), &e.to_string())
}

/// The one `MaintenanceFailed` condition body, so the two constructors above
/// cannot drift in reason, wording or timestamp shape.
fn maintenance_failure_body(
    class: kopiur_kopia::KopiaErrorClass,
    message: &str,
) -> serde_json::Value {
    serde_json::json!({
        "status": {
            "conditions": [lease_condition_body(
                "False",
                "MaintenanceFailed",
                &format!("maintenance failed (class {class}): {message}"),
                &chrono::Utc::now(),
            )],
        }
    })
}

/// A single `LeaseOwned` condition. The codebase uses a single-element
/// `conditions` array (last-writer-wins for the salient state) for `Maintenance`.
pub fn lease_condition_body(
    status: &str,
    reason: &str,
    message: &str,
    now: &chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    serde_json::json!({
        "type": kopiur_api::maintenance::LEASE_OWNED_CONDITION,
        "status": status,
        "reason": reason,
        "message": message,
        "lastTransitionTime": now.to_rfc3339(),
        "observedGeneration": 0,
    })
}

/// A thin, best-effort wrapper around the kube status PATCH. Kept separate from
/// the pure mapping so `main`'s correctness lives in the unit-tested layers.
/// When no cluster is reachable, status updates are logged instead.
pub struct StatusReporter {
    inner: Option<Arc<Mutex<KubeStatusReporter>>>,
    target: workspec::TargetRef,
}

impl StatusReporter {
    /// Build a reporter that ALWAYS logs and NEVER PATCHes, regardless of
    /// whether an in-cluster kube client happens to be reachable.
    ///
    /// Unlike [`Self::try_new`]'s best-effort fallback (which only degrades
    /// to logging when a kube client can't be built), this never even
    /// attempts one — for operations whose `targetRef` names no CR the
    /// controller owns and for which the mover is deliberately NOT granted
    /// RBAC to PATCH (e.g. `SnapshotDeleteBatch`, whose `targetRef` names the
    /// Job itself).
    pub fn log_only(target: workspec::TargetRef) -> Self {
        StatusReporter {
            inner: None,
            target,
        }
    }

    /// Build a reporter for the work spec's target, falling back to a
    /// log-only reporter when no kube client is reachable.
    pub async fn try_new(spec: &MoverWorkSpec) -> Self {
        let target = spec.target_ref.clone();
        match KubeStatusReporter::try_new(&target).await {
            Ok(r) => StatusReporter {
                inner: Some(Arc::new(Mutex::new(r))),
                target,
            },
            Err(e) => {
                warn!(
                    error = %e,
                    "no kube client; status updates will be logged, not PATCHed"
                );
                StatusReporter {
                    inner: None,
                    target,
                }
            }
        }
    }

    /// Report a status update — PATCHed to the cluster when a client is
    /// available, otherwise logged (best-effort; PATCH failures are logged).
    pub async fn report(&self, update: &StatusUpdate) {
        match &self.inner {
            Some(r) => {
                let mut guard = r.lock().await;
                if let Err(e) = guard.patch(update).await {
                    warn!(error = %e, target = %self.target.name, "status PATCH failed");
                }
            }
            None => {
                info!(
                    target = %self.target.name,
                    phase = update.phase.as_deref().unwrap_or("progress"),
                    "status update (no cluster): {}",
                    serde_json::to_string(update).unwrap_or_default()
                );
            }
        }
    }

    /// Read the target's currently-pinned `status.resolved`, if any. Used by the
    /// in-Job restore resolver to reuse a snapshot a PRIOR pod attempt already
    /// pinned (deterministic across Job retries) instead of re-resolving "latest".
    /// The read goes through the `/status` subresource, the one resource the
    /// least-privilege mover role grants `get` on (#401 — a base-object GET is
    /// Forbidden under that role). Best-effort: returns `None` when there's no
    /// client or the read fails.
    pub async fn resolved(&self) -> Option<ResolvedRestore> {
        let r = self.inner.as_ref()?;
        let guard = r.lock().await;
        match guard.read_resolved().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, target = %self.target.name, "status.resolved read failed");
                None
            }
        }
    }

    /// Pin `resolved` to the target's `status.resolved` via a resolved-only merge
    /// PATCH (no `phase`, so it never trips the Restore phase enum). The in-Job
    /// resolver calls this BEFORE restoring so the chosen snapshot is durably
    /// recorded — a retry reuses it, the controller adopts it (pin-once), and the
    /// outcome survives a later failure of the terminal status PATCH. Best-effort.
    pub async fn pin_resolved(&self, resolved: &ResolvedRestore) {
        match &self.inner {
            Some(r) => {
                let mut guard = r.lock().await;
                if let Err(e) = guard.pin_resolved(resolved).await {
                    warn!(error = %e, target = %self.target.name, "status.resolved pin failed");
                }
            }
            None => info!(
                target = %self.target.name,
                "status.resolved pin (no cluster): {}",
                serde_json::to_string(resolved).unwrap_or_default()
            ),
        }
    }
}

/// The real kube PATCH path. Uses a dynamic API so the mover does not need to
/// depend on the typed CRD structs (it PATCHes a merge body under `.status`).
pub struct KubeStatusReporter {
    api: kube::Api<kube::api::DynamicObject>,
    kind: String,
    namespace: String,
    name: String,
}

impl KubeStatusReporter {
    /// Build the dynamic-API reporter for `target`, erroring when no kube
    /// client can be constructed.
    pub async fn try_new(target: &workspec::TargetRef) -> Result<Self> {
        let client =
            kube::Client::try_default()
                .await
                .map_err(|source| MoverError::KubeClient {
                    source: Box::new(source),
                })?;
        Ok(Self::from_client(client, target))
    }

    /// Build the reporter around an existing client. The seam the unit tests
    /// inject a mock service through ([`kube::Client::new`]); production goes
    /// via [`Self::try_new`].
    fn from_client(client: kube::Client, target: &workspec::TargetRef) -> Self {
        use kube::core::{ApiResource, GroupVersionKind};

        let (group, version) = split_api_version(&target.api_version);
        let gvk = GroupVersionKind::gvk(&group, &version, &target.kind);
        let ar = ApiResource::from_gvk(&gvk);
        // ClusterRepository is the one cluster-scoped mover target kind: a
        // namespaced API would build `/namespaces/<ns>/clusterrepositories/...`,
        // a path that does not exist, so every status call would 404 into the
        // best-effort warn. No operation reaches this today (the only
        // ClusterRepository targetRef is the bootstrap work-spec, and the
        // bootstrap flow reports via its result ConfigMap, never a
        // StatusReporter) — this keeps the trap from going live with a future op.
        let api = if target.kind == "ClusterRepository" {
            kube::Api::<kube::api::DynamicObject>::all_with(client, &ar)
        } else {
            kube::Api::<kube::api::DynamicObject>::namespaced_with(client, &target.namespace, &ar)
        };
        KubeStatusReporter {
            api,
            kind: target.kind.clone(),
            namespace: target.namespace.clone(),
            name: target.name.clone(),
        }
    }

    /// PATCH the update's `.status` merge body onto the target object.
    pub async fn patch(&mut self, update: &StatusUpdate) -> Result<()> {
        use kube::api::{Patch, PatchParams};
        let body = update.as_patch_body();
        self.api
            .patch_status(&self.name, &PatchParams::default(), &Patch::Merge(&body))
            .await
            .map_err(|source| MoverError::StatusPatch {
                kind: self.kind.clone(),
                namespace: self.namespace.clone(),
                name: self.name.clone(),
                source: Box::new(source),
            })?;
        Ok(())
    }

    /// GET the target THROUGH THE `/status` SUBRESOURCE (which returns the full
    /// object) and deserialize its `status.resolved`, if present.
    ///
    /// The subresource route is load-bearing, not a style choice (#401): the
    /// least-privilege mover role grants `get` only on `{crd}/status`, never on
    /// the base resource, so a base-object GET is Forbidden in every real
    /// install and the retry-determinism this read exists for silently never
    /// works. A NotFound (CR deleted mid-run) is "no pin", matching the old
    /// `get_opt` semantics; any other failure is a [`MoverError::StatusRead`]
    /// so the log names the rejected GET rather than a PATCH.
    async fn read_resolved(&self) -> Result<Option<ResolvedRestore>> {
        let obj = match self.api.get_status(&self.name).await {
            Ok(obj) => Some(obj),
            // `is_not_found()` (reason == "NotFound"), not a bare 404 check:
            // a non-NotFound 404 (e.g. the CRD or API path absent) is a real
            // misconfiguration and must surface, not read as "no pin yet".
            Err(kube::Error::Api(status)) if status.is_not_found() => None,
            Err(source) => {
                return Err(MoverError::StatusRead {
                    kind: self.kind.clone(),
                    namespace: self.namespace.clone(),
                    name: self.name.clone(),
                    source: Box::new(source),
                });
            }
        };
        Ok(obj
            .and_then(|o| {
                o.data
                    .get("status")
                    .and_then(|s| s.get("resolved"))
                    .cloned()
            })
            .and_then(|v| serde_json::from_value(v).ok()))
    }

    /// Resolved-only `.status` merge PATCH (no `phase`), pinning `status.resolved`.
    async fn pin_resolved(&mut self, resolved: &ResolvedRestore) -> Result<()> {
        use kube::api::{Patch, PatchParams};
        let body = serde_json::json!({ "status": { "resolved": resolved } });
        self.api
            .patch_status(&self.name, &PatchParams::default(), &Patch::Merge(&body))
            .await
            .map_err(|source| MoverError::StatusPatch {
                kind: self.kind.clone(),
                namespace: self.namespace.clone(),
                name: self.name.clone(),
                source: Box::new(source),
            })?;
        Ok(())
    }
}

/// Split `group/version` (or bare `version`) into `(group, version)`.
pub fn split_api_version(api_version: &str) -> (String, String) {
    match api_version.split_once('/') {
        Some((g, v)) => (g.to_string(), v.to_string()),
        None => (String::new(), api_version.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_kopia::KopiaErrorClass;

    fn ts() -> DateTime<Utc> {
        "2026-06-01T12:00:00Z".parse().unwrap()
    }

    /// #374: a maintenance run can now fail on the repository THROTTLE, whose
    /// typed cause is a `MoverError`. Both constructors must produce the same
    /// `MaintenanceFailed` condition shape and carry the real class, so the
    /// controller's retry hint is identical whichever step failed.
    #[test]
    fn both_maintenance_failure_bodies_share_one_condition_shape() {
        let kopia = KopiaError::NonZeroExit {
            args: "maintenance run".into(),
            code: Some(1),
            class: KopiaErrorClass::Locked,
            stderr_tail: "repository is locked".into(),
        };
        let from_kopia = maintenance_failed_body(&kopia);
        let from_mover = maintenance_failed_body_from_mover(&crate::error::MoverError::Kopia {
            op: crate::error::KopiaOp::ThrottleSet,
            source: KopiaError::NonZeroExit {
                args: "repository throttle set".into(),
                code: Some(1),
                class: KopiaErrorClass::Locked,
                stderr_tail: "repository is locked".into(),
            },
        });
        for body in [&from_kopia, &from_mover] {
            let cond = &body["status"]["conditions"][0];
            assert_eq!(cond["type"], kopiur_api::maintenance::LEASE_OWNED_CONDITION);
            assert_eq!(cond["status"], "False");
            assert_eq!(cond["reason"], "MaintenanceFailed");
            assert!(
                cond["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("class Locked")),
                "the condition message must carry the real kopia class: {cond}"
            );
        }
        assert!(
            from_mover["status"]["conditions"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains("repository throttle set")),
            "a throttle failure must name the invocation that failed"
        );
    }

    #[test]
    fn stats_from_create_result_uses_crd_field_names() {
        let json = r#"{
            "id":"x","source":{"host":"h","userName":"u","path":"/p"},
            "startTime":"2026-06-02T03:13:59Z","endTime":"2026-06-02T03:14:00Z",
            "rootEntry":{"name":"p","type":"d","obj":"k1","summ":{"size":100,"files":5,"dirs":2,"numFailed":1}}
        }"#;
        let r: SnapshotCreateResult = serde_json::from_str(json).unwrap();
        let stats = stats_from_result(&r);
        assert_eq!(stats.size_bytes, Some(100));
        assert_eq!(stats.files_new, Some(5));
        // The serialized body MUST use the CRD `status.stats` field names, or the
        // API server prunes them and the stats are lost (regression guard).
        let body = serde_json::to_value(&stats).unwrap();
        assert_eq!(body["sizeBytes"], 100);
        assert_eq!(body["filesNew"], 5);
        assert!(body.get("totalBytes").is_none(), "stale field name leaked");
    }

    #[test]
    fn timing_from_create_result_computes_duration() {
        let json = r#"{
            "id":"x","source":{"host":"h","userName":"u","path":"/p"},
            "startTime":"2026-06-02T03:13:59Z","endTime":"2026-06-02T03:14:00Z",
            "rootEntry":{"name":"p","type":"d","obj":"k1","summ":{"size":1,"files":1}}
        }"#;
        let r: SnapshotCreateResult = serde_json::from_str(json).unwrap();
        let timing = timing_from_result(&r);
        assert_eq!(timing.duration_seconds, Some(1));
    }

    #[test]
    fn failure_block_from_nonzero_exit_retryable() {
        let err = KopiaError::NonZeroExit {
            args: "snapshot create".into(),
            code: Some(1),
            class: KopiaErrorClass::RepositoryUnavailable,
            stderr_tail: "error connecting to repository: dial tcp".into(),
        };
        let fb = failure_block_from_kopia(&err);
        assert_eq!(fb.kopia_error_class, "RepositoryUnavailable");
        assert_eq!(fb.exit_code, Some(1));
        assert_eq!(
            fb.stderr_tail.as_deref(),
            Some("error connecting to repository: dial tcp")
        );
        assert!(fb.retry_recommended);
    }

    #[test]
    fn failure_block_from_auth_not_retryable() {
        let err = KopiaError::NonZeroExit {
            args: "repository connect".into(),
            code: Some(1),
            class: KopiaErrorClass::AuthFailure,
            stderr_tail: "invalid repository password".into(),
        };
        let fb = failure_block_from_kopia(&err);
        assert_eq!(fb.kopia_error_class, "AuthFailure");
        assert!(!fb.retry_recommended);
    }

    #[test]
    fn failure_block_from_spawn_error_no_exit_code() {
        let err = KopiaError::Spawn {
            binary: "kopia".into(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        let fb = failure_block_from_kopia(&err);
        assert_eq!(fb.kopia_error_class, "Unknown");
        assert_eq!(fb.exit_code, None);
        assert_eq!(fb.stderr_tail, None);
        assert!(!fb.retry_recommended);
    }

    #[test]
    fn failure_block_from_timeout_retryable() {
        let err = KopiaError::Timeout {
            args: "snapshot create".into(),
            seconds: 3600,
        };
        let fb = failure_block_from_kopia(&err);
        assert_eq!(fb.kopia_error_class, "RepositoryUnavailable");
        assert!(fb.retry_recommended);
    }

    #[test]
    fn failure_block_from_mover_kopia_matches_the_kopia_error_path() {
        // The MoverError wrapper must not lose anything the bare-KopiaError
        // mapping carried: class, stderr tail, exit code, retry hint all
        // survive; only the message gains the "which op" prefix.
        use crate::error::{KopiaOp, MoverError};
        let kopia = KopiaError::NonZeroExit {
            args: "snapshot create".into(),
            code: Some(1),
            class: KopiaErrorClass::RepositoryUnavailable,
            stderr_tail: "dial tcp: connection refused".into(),
        };
        let bare = failure_block_from_kopia(&kopia);
        let wrapped = FailureBlock::from(&MoverError::Kopia {
            op: KopiaOp::SnapshotCreate,
            source: kopia,
        });
        assert_eq!(wrapped.kopia_error_class, bare.kopia_error_class);
        assert_eq!(wrapped.stderr_tail, bare.stderr_tail);
        assert_eq!(wrapped.exit_code, bare.exit_code);
        assert_eq!(wrapped.retry_recommended, bare.retry_recommended);
        assert!(wrapped.message.starts_with("snapshot create failed"));
        // The wrapper is the one place the op label is known — the bare kopia
        // path has no KopiaOp in scope, so its block carries no op.
        assert_eq!(wrapped.op.as_deref(), Some("snapshot create"));
        assert_eq!(bare.op, None);
    }

    #[test]
    fn failure_block_op_is_the_stable_kopia_op_label() {
        // The controller's repository-shaped gate keys on the persisted op
        // label (#345): a repository-connect failure must land as EXACTLY
        // `repository connect` — the value of KopiaOp::as_str() — and its
        // serialized field name must be the CRD's `op` (a drifting name is
        // silently pruned by the API server).
        use crate::error::{KopiaOp, MoverError};
        let err = MoverError::Kopia {
            op: KopiaOp::RepositoryConnect,
            source: KopiaError::NonZeroExit {
                args: "repository connect".into(),
                code: Some(1),
                class: KopiaErrorClass::RepositoryUnavailable,
                stderr_tail: "dial tcp: connection refused".into(),
            },
        };
        let fb = FailureBlock::from(&err);
        assert_eq!(fb.op.as_deref(), Some("repository connect"));
        let body = StatusUpdate::failed_mover(&err, ts()).as_patch_body();
        assert_eq!(body["status"]["failure"]["op"], "repository connect");
    }

    #[test]
    fn failure_block_from_non_kopia_mover_error_carries_no_op() {
        // Failures outside a kopia invocation have no op to record — the field
        // must stay absent (None serializes to nothing), never a bogus label.
        use crate::error::MoverError;
        let fb = FailureBlock::from(&MoverError::WorkSpecPathMissing);
        assert_eq!(fb.op, None);
        let body =
            StatusUpdate::failed_mover(&MoverError::WorkSpecPathMissing, ts()).as_patch_body();
        assert!(body["status"]["failure"].get("op").is_none());
    }

    #[test]
    fn failure_block_from_bootstrap_failed_keeps_class_and_retry_hint() {
        use crate::error::MoverError;
        let fb = FailureBlock::from(&MoverError::BootstrapFailed {
            class: KopiaErrorClass::AuthFailure,
            message: "invalid repository password".into(),
        });
        assert_eq!(fb.kopia_error_class, "AuthFailure");
        assert!(!fb.retry_recommended);
        assert!(fb.stderr_tail.is_none());
    }

    #[test]
    fn failure_block_from_environmental_mover_error_is_unknown_non_retryable() {
        use crate::error::MoverError;
        let fb = FailureBlock::from(&MoverError::WorkSpecPathMissing);
        assert_eq!(fb.kopia_error_class, "Unknown");
        assert!(!fb.retry_recommended);
        assert!(fb.exit_code.is_none());
        assert!(fb.message.contains("KOPIUR_WORK_SPEC_PATH"));
    }

    #[test]
    fn unchanged_backup_reports_success_with_no_manifest() {
        // #351. Three things must all hold at once, and each is load-bearing:
        let start = ts();
        let end = start + chrono::Duration::seconds(9);
        let u = StatusUpdate::unchanged_backup(start, end);

        // 1. A distinct terminal phase, not `Succeeded`. Reporting Succeeded
        //    would send the controller looking for "its" snapshot, and the
        //    newest one matching this identity belongs to the PREVIOUS CR.
        assert_eq!(u.phase.as_deref(), Some("Unchanged"));

        // 2. NO snapshot block — this run owns no kopia manifest, so it must not
        //    record an id it could later delete from under its owner.
        assert!(
            u.snapshot.is_none(),
            "an Unchanged run must not claim a kopia snapshot id"
        );
        assert!(u.stats.is_none(), "no manifest means no stats to report");

        // 3. Timing IS present. Without an endTime the policy's last-backup
        //    timestamp never advances and KopiurBackupStale pages for a source
        //    that is simply not changing.
        let t = u.timing.as_ref().expect("timing present");
        assert_eq!(t.end_time.as_deref(), Some(end.to_rfc3339()).as_deref());
        assert_eq!(t.duration_seconds, Some(9));

        assert!(u.failure.is_none(), "a dedupe is not a failure");
    }

    #[test]
    fn succeeded_backup_update() {
        let json = r#"{
            "id":"snap1","source":{"host":"h","userName":"u","path":"/p"},
            "startTime":"2026-06-02T03:13:59Z","endTime":"2026-06-02T03:14:00Z",
            "rootEntry":{"name":"p","type":"d","obj":"k1","summ":{"size":42,"files":3}}
        }"#;
        let r: SnapshotCreateResult = serde_json::from_str(json).unwrap();
        let u = StatusUpdate::succeeded_backup(&r, ts());
        assert_eq!(u.phase.as_deref(), Some("Succeeded"));
        // The snapshot MUST serialize as the nested CRD shape
        // `status.snapshot.kopiaSnapshotID`, or the API server prunes it (the bug
        // that left object-store backups Succeeded with no snapshot id).
        let snap = u.snapshot.as_ref().expect("snapshot present");
        assert_eq!(snap.kopia_snapshot_id, "snap1");
        assert_eq!(snap.identity.username, "u");
        assert_eq!(snap.identity.hostname, "h");
        let body = u.as_patch_body();
        assert_eq!(body["status"]["snapshot"]["kopiaSnapshotID"], "snap1");
        assert!(
            body["status"].get("snapshotId").is_none(),
            "flat snapshotId leaked; the API server would prune it"
        );
        assert_eq!(u.stats.as_ref().unwrap().size_bytes, Some(42));
        assert_eq!(u.stats.as_ref().unwrap().files_new, Some(3));
        // A clean snapshot has no excluded entries → filesFailed stays absent.
        assert_eq!(u.stats.as_ref().unwrap().files_failed, None);
        assert!(u.timing.is_some());
        assert!(u.failure.is_none());
        // No description recorded → the field is elided from the PATCH entirely.
        assert!(snap.description.is_none());
        assert!(body["status"]["snapshot"].get("description").is_none());
    }

    #[test]
    fn succeeded_backup_surfaces_the_kopia_description() {
        // `snapshot create --description` comes back on the create result; the
        // status PATCH must carry it under the CRD's `status.snapshot.description`.
        let json = r#"{
            "id":"snap3","source":{"host":"h","userName":"u","path":"/p"},
            "description":"pre-upgrade snapshot",
            "startTime":"2026-06-02T03:13:59Z","endTime":"2026-06-02T03:14:00Z"
        }"#;
        let r: SnapshotCreateResult = serde_json::from_str(json).unwrap();
        let u = StatusUpdate::succeeded_backup(&r, ts());
        assert_eq!(
            u.as_patch_body()["status"]["snapshot"]["description"],
            "pre-upgrade snapshot"
        );
    }

    #[test]
    fn succeeded_backup_surfaces_excluded_entry_count() {
        // ignore-file-errors path: exit 0, numFailed 0, but summ.errors[] lists the
        // skipped entries → the backup is incomplete and `filesFailed` carries the count.
        let json = r#"{
            "id":"snap2","source":{"host":"h","userName":"u","path":"/pvc"},
            "startTime":"2026-06-02T03:13:59Z","endTime":"2026-06-02T03:14:00Z",
            "rootEntry":{"name":"pvc","type":"d","obj":"k1","summ":{"size":7,"files":2,"numFailed":0,
                "errors":[
                    {"path":"secret_dir","error":"unable to read directory: permission denied"},
                    {"path":"topsecret.txt","error":"unable to open file: permission denied"}
                ]}}
        }"#;
        let r: SnapshotCreateResult = serde_json::from_str(json).unwrap();
        let u = StatusUpdate::succeeded_backup(&r, ts());
        assert_eq!(u.phase.as_deref(), Some("Succeeded"));
        assert_eq!(
            u.stats.as_ref().unwrap().files_failed,
            Some(2),
            "the count of EXCLUDED source entries must ride on status.stats.filesFailed"
        );
        assert_eq!(u.as_patch_body()["status"]["stats"]["filesFailed"], 2);
    }

    #[test]
    fn succeeded_pin_restamps_snapshot_id_only() {
        // After a pin, kopia rewrote the manifest id; the pin update must carry
        // the CURRENT id under the nested CRD shape so the finalizer delete and
        // snapshotRef restore target the live manifest — and ONLY status.snapshot
        // (no timing/stats) so the Merge PATCH never disturbs create-time fields,
        // and status.pinned stays the controller's to write (no two-writer churn).
        let info = SnapshotInfo {
            kopia_snapshot_id: "b2037e14".into(),
            identity: ResolvedIdentity {
                username: "home-wyoming-whisper".into(),
                hostname: "home".into(),
                source_path: Some("/pvc/wyoming-whisper".into()),
            },
            description: None,
        };
        let u = StatusUpdate::succeeded_pin(info, ts());
        assert_eq!(u.phase.as_deref(), Some("Succeeded"));
        let body = u.as_patch_body();
        assert_eq!(body["status"]["snapshot"]["kopiaSnapshotID"], "b2037e14");
        assert_eq!(
            body["status"]["snapshot"]["identity"]["sourcePath"],
            "/pvc/wyoming-whisper"
        );
        // Disjoint subtrees: the pin update must NOT touch timing/stats/pinned.
        assert!(u.timing.is_none());
        assert!(u.stats.is_none());
        assert!(body["status"].get("timing").is_none());
        assert!(body["status"].get("stats").is_none());
        assert!(body["status"].get("pinned").is_none());
        assert!(u.failure.is_none());
    }

    #[test]
    fn restore_terminal_phase_is_completed_not_succeeded() {
        // Regression: the mover used `succeeded()` ("Succeeded") for restores,
        // but the Restore CRD enum only allows "Completed", so the status PATCH
        // was rejected 422 and every restore flooded the controller logs. The
        // restore terminal phase MUST match RestorePhase::Completed.
        let u = StatusUpdate::completed("k1f1ec0a8", ts());
        assert_eq!(u.phase.as_deref(), Some("Completed"));
        assert_eq!(u.phase.as_deref(), Some(RestorePhase::Completed.label()));
        assert_ne!(u.phase.as_deref(), Some(MoverPhase::Succeeded.as_str()));
        assert!(u.failure.is_none());
        assert!(u.snapshot.is_none());
        let body = u.as_patch_body();
        assert_eq!(body["status"]["phase"], "Completed");
        // The restored snapshot id is surfaced on logTail (the exact CRD field
        // name — a drifting name would be pruned by the API server).
        assert_eq!(
            body["status"]["logTail"],
            "Restore completed: snapshot k1f1ec0a8"
        );
    }

    #[test]
    fn completed_resolved_pins_status_resolved_for_in_job_resolution() {
        // The object-store in-Job path: the mover resolved the snapshot itself, so
        // it must pin status.resolved (the controller couldn't list the backend).
        let identity = ResolvedIdentity {
            username: "restore".into(),
            hostname: "prod".into(),
            source_path: Some("/pvc/db".into()),
        };
        let u = StatusUpdate::completed_resolved("k9", identity, ts());
        assert_eq!(u.phase.as_deref(), Some("Completed"));
        let body = u.as_patch_body();
        // The exact CRD `status.resolved` field names, or the API server prunes them.
        assert_eq!(body["status"]["resolved"]["resolution"], "Snapshot");
        assert_eq!(body["status"]["resolved"]["kopiaSnapshotID"], "k9");
        assert_eq!(
            body["status"]["resolved"]["identity"]["sourcePath"],
            "/pvc/db"
        );
        assert!(body["status"]["resolved"]["pinnedAt"].is_string());
        assert_eq!(body["status"]["logTail"], "Restore completed: snapshot k9");
    }

    #[test]
    fn completed_empty_pins_no_snapshot_outcome() {
        // Deploy-or-restore with no match under Continue: pin NoSnapshot so a
        // later-appearing snapshot can never silently retarget the Restore.
        let u = StatusUpdate::completed_empty(ts());
        assert_eq!(u.phase.as_deref(), Some("Completed"));
        let body = u.as_patch_body();
        assert_eq!(body["status"]["resolved"]["resolution"], "NoSnapshot");
        assert!(body["status"]["resolved"].get("kopiaSnapshotID").is_none());
        assert!(
            body["status"]["logTail"]
                .as_str()
                .unwrap()
                .contains("deploy-or-restore")
        );
    }

    #[test]
    fn failed_update_carries_block() {
        let err = KopiaError::EmptyOutput {
            context: "snapshot create result".into(),
            stderr_tail: String::new(),
        };
        let u = StatusUpdate::failed(&err, ts());
        assert_eq!(u.phase.as_deref(), Some("Failed"));
        assert!(u.failure.is_some());
        assert_eq!(u.failure.unwrap().kopia_error_class, "Unknown");
    }

    #[test]
    fn patch_body_wraps_under_status() {
        let u = StatusUpdate::succeeded(ts());
        let body = u.as_patch_body();
        assert_eq!(body["status"]["phase"], "Succeeded");
    }

    #[test]
    fn progress_heartbeat_carries_no_phase() {
        // Regression (the Restore "Running" 422): the periodic heartbeat used to
        // hardcode phase: "Running", valid for SnapshotPhase but rejected by the
        // RestorePhase enum, so every restore flooded the controller logs. The
        // heartbeat MUST NOT assert a phase at all — the controller owns every
        // in-flight phase — so the update can never be invalid for ANY CR.
        let u = StatusUpdate::progress(ts());
        assert_eq!(u.phase, None);
        let body = u.as_patch_body();
        assert!(
            body["status"].get("phase").is_none(),
            "the progress heartbeat must omit phase entirely"
        );
        // It also never sets terminal fields (the status-churn rule).
        assert!(body["status"].get("logTail").is_none());
        assert!(body["status"].get("failure").is_none());
    }

    #[test]
    fn capped_tail_keeps_the_last_bytes_on_char_and_line_boundaries() {
        use kopiur_api::common::MAX_LOG_TAIL_BYTES;
        // Under the cap: passthrough.
        assert_eq!(capped_tail("short"), "short");
        // Exactly at the cap: passthrough.
        let exact = "x".repeat(MAX_LOG_TAIL_BYTES);
        assert_eq!(capped_tail(&exact), exact);
        // Over the cap: keeps the LAST bytes (the newest output), bounded.
        let over = format!("{}{}", "a".repeat(MAX_LOG_TAIL_BYTES), "tail-marker");
        let capped = capped_tail(&over);
        assert!(capped.len() <= MAX_LOG_TAIL_BYTES);
        assert!(capped.ends_with("tail-marker"));
        // Multi-byte char straddling the cut: never panics, stays valid UTF-8.
        let snowmen = "☃".repeat(MAX_LOG_TAIL_BYTES); // 3 bytes each
        let capped = capped_tail(&snowmen);
        assert!(capped.len() <= MAX_LOG_TAIL_BYTES);
        assert!(capped.chars().all(|c| c == '☃'));
        // A newline shortly after the cut: the tail starts on a whole line
        // (a partial first line is dropped when a full one follows close by).
        let payload = format!("fresh line{}", "x".repeat(MAX_LOG_TAIL_BYTES - 60));
        let lines = format!("{}\n{}", "junk".repeat(2000), payload);
        assert!(capped_tail(&lines).starts_with("fresh line"));
    }

    #[test]
    fn terminal_updates_carry_log_tail_with_the_exact_crd_field_name() {
        // Success: the documented `Snapshot created: <id>` line (ADR §3.4),
        // serialized as `logTail` — the CRD's field name. A drifting name is
        // silently pruned by the API server (regression guard).
        let json = r#"{
            "id":"snap1","source":{"host":"h","userName":"u","path":"/p"},
            "startTime":"2026-06-02T03:13:59Z","endTime":"2026-06-02T03:14:00Z",
            "rootEntry":{"name":"p","type":"d","obj":"k1","summ":{"size":42,"files":3}}
        }"#;
        let r: SnapshotCreateResult = serde_json::from_str(json).unwrap();
        let body = StatusUpdate::succeeded_backup(&r, ts()).as_patch_body();
        assert_eq!(body["status"]["logTail"], "Snapshot created: snap1");

        // Failure: logTail carries the actionable message + kopia stderr tail,
        // alongside the structured failure block.
        let err = KopiaError::NonZeroExit {
            args: "repository connect".into(),
            code: Some(1),
            class: KopiaErrorClass::AuthFailure,
            stderr_tail: "invalid repository password".into(),
        };
        let body = StatusUpdate::failed(&err, ts()).as_patch_body();
        let tail = body["status"]["logTail"].as_str().unwrap();
        assert!(tail.contains("invalid repository password"), "{tail}");
        assert_eq!(
            body["status"]["failure"]["kopiaErrorClass"], "AuthFailure",
            "the structured failure block must land under the CRD's field names"
        );

        // Progress updates never set logTail (it is written once, at the
        // terminal transition — the status-churn rule).
        let body = StatusUpdate::progress(ts()).as_patch_body();
        assert!(body["status"].get("logTail").is_none());
    }

    #[test]
    fn log_only_reporter_never_carries_a_kube_client() {
        // The whole point of `log_only`: no kube client is ever built, so
        // `.report()`/`.pin_resolved()` can only ever log — ​never PATCH —
        // regardless of whether a client happens to be reachable in-cluster.
        // (RBAC is deliberately not granted for this target, e.g.
        // SnapshotDeleteBatch's targetRef names the Job itself.)
        let target = workspec::TargetRef {
            api_version: "kopiur.home-operations.com/v1alpha1".into(),
            kind: "SnapshotDeleteBatch".into(),
            name: "prune-job".into(),
            namespace: "backups".into(),
        };
        let reporter = StatusReporter::log_only(target.clone());
        assert!(reporter.inner.is_none());
        assert_eq!(reporter.target.name, target.name);
        assert_eq!(reporter.target.kind, target.kind);
    }

    #[test]
    fn split_api_version_grouped() {
        assert_eq!(
            split_api_version("kopiur.home-operations.com/v1alpha1"),
            (
                "kopiur.home-operations.com".to_string(),
                "v1alpha1".to_string()
            )
        );
    }

    #[test]
    fn split_api_version_core() {
        assert_eq!(split_api_version("v1"), (String::new(), "v1".to_string()));
    }

    fn maint_op(mode: MaintenanceMode) -> MaintenanceOp {
        MaintenanceOp {
            mode,
            owner: "kopiur/prod/nas".into(),
            owner_aliases: Vec::new(),
            takeover_policy: kopiur_api::TakeoverPolicy::Never,
        }
    }

    #[test]
    fn quick_run_advances_only_quick_clock() {
        let now = chrono::Utc::now();
        let body = maintenance_ran_body(&maint_op(MaintenanceMode::Quick), &now);
        assert!(body["status"]["quick"]["lastRunAt"].is_string());
        assert!(
            body["status"]["full"].is_null(),
            "a quick run must not stamp the full clock"
        );
        assert_eq!(body["status"]["ownership"]["owner"], "kopiur/prod/nas");
    }

    #[test]
    fn full_run_subsumes_quick_clock() {
        let now = chrono::Utc::now();
        let body = maintenance_ran_body(&maint_op(MaintenanceMode::Full), &now);
        // Full subsumes quick: both clocks advance so quick isn't immediately due.
        assert!(body["status"]["full"]["lastRunAt"].is_string());
        assert!(body["status"]["quick"]["lastRunAt"].is_string());
        assert_eq!(
            body["status"]["full"]["lastRunAt"],
            body["status"]["quick"]["lastRunAt"]
        );
    }

    #[test]
    fn snapshot_replicate_ok_body_carries_last_run_and_ready_true() {
        let stats = SnapshotReplicationRunStats {
            identities_selected: 3,
            snapshots_copied: 5,
            already_present: 2,
            failed: 0,
            pruned: 1,
        };
        let body = snapshot_replicate_ok_body(
            "S3",
            &ts(),
            &stats,
            "ReplicationSucceeded",
            "replicated 5 snapshot(s)",
        );
        assert_eq!(body["status"]["phase"], "Succeeded");
        assert_eq!(body["status"]["lastReplicated"], ts().to_rfc3339());
        // lastRun's exact camelCase field names — the apiserver prunes drift.
        assert_eq!(body["status"]["lastRun"]["identitiesSelected"], 3);
        assert_eq!(body["status"]["lastRun"]["snapshotsCopied"], 5);
        assert_eq!(body["status"]["lastRun"]["alreadyPresent"], 2);
        assert_eq!(body["status"]["lastRun"]["failed"], 0);
        assert_eq!(body["status"]["lastRun"]["pruned"], 1);
        let cond = &body["status"]["conditions"][0];
        assert_eq!(cond["type"], "Ready");
        assert_eq!(cond["status"], "True");
        assert_eq!(cond["reason"], "ReplicationSucceeded");
        assert!(
            cond["message"]
                .as_str()
                .unwrap()
                .contains("destination: S3"),
            "{cond}"
        );
        // observedGeneration stays 0: the controller heals it next pass.
        assert_eq!(cond["observedGeneration"], 0);
    }

    #[test]
    fn snapshot_replicate_ok_body_supports_the_no_match_reason() {
        let body = snapshot_replicate_ok_body(
            "Filesystem",
            &ts(),
            &SnapshotReplicationRunStats::default(),
            "NoIdentitiesMatched",
            "no source identities matched the selection; nothing to replicate",
        );
        assert_eq!(body["status"]["phase"], "Succeeded");
        assert_eq!(
            body["status"]["conditions"][0]["reason"],
            "NoIdentitiesMatched"
        );
        assert_eq!(body["status"]["lastRun"]["identitiesSelected"], 0);
    }

    #[test]
    fn snapshot_replicate_failed_body_is_ready_false_with_optional_stats() {
        // Without stats (a failure before any counters existed).
        let body = snapshot_replicate_failed_body("source password probe failed", None);
        assert_eq!(body["status"]["phase"], "Failed");
        let cond = &body["status"]["conditions"][0];
        assert_eq!(cond["type"], "Ready");
        assert_eq!(cond["status"], "False");
        assert_eq!(cond["reason"], "ReplicationFailed");
        assert_eq!(cond["message"], "source password probe failed");
        assert!(body["status"].get("lastRun").is_none());
        assert!(body["status"].get("lastReplicated").is_none());

        // With stats (post-verify misses carry the failed count).
        let stats = SnapshotReplicationRunStats {
            identities_selected: 2,
            snapshots_copied: 1,
            already_present: 0,
            failed: 3,
            pruned: 0,
        };
        let body = snapshot_replicate_failed_body("3 missing", Some(&stats));
        assert_eq!(body["status"]["lastRun"]["failed"], 3);
        assert_eq!(body["status"]["lastRun"]["snapshotsCopied"], 1);
    }

    #[test]
    fn lease_blocked_records_observed_owner_and_false_condition() {
        let body = lease_blocked_body("other/owner", "LeaseHeldByOther", "held");
        assert_eq!(body["status"]["ownership"]["owner"], "other/owner");
        assert_eq!(body["status"]["conditions"][0]["status"], "False");
        assert_eq!(body["status"]["conditions"][0]["type"], "LeaseOwned");
    }

    // --- §4 verify status bodies (single-repo flat vs #368 entry-keyed) -------

    #[test]
    fn verify_ok_body_single_repo_stays_flat_and_byte_identical() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-01T00:00:00+00:00")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let body = verify_ok_body("quick", None, &now);
        assert_eq!(body["status"]["lastVerified"], "2026-08-01T00:00:00+00:00");
        assert_eq!(body["status"]["conditions"][0]["type"], "Verified");
        assert_eq!(body["status"]["conditions"][0]["status"], "True");
        assert!(
            body["status"].get("verificationStamps").is_none(),
            "single-repo must never write the multi-repo stamp map"
        );
    }

    #[test]
    fn verify_ok_body_multi_repo_writes_only_its_own_stamp_key() {
        let now = chrono::Utc::now();
        let body = verify_ok_body("deep", Some("Repository/backups/nas"), &now);
        let stamps = &body["status"]["verificationStamps"];
        assert!(stamps["Repository/backups/nas"].is_string());
        assert!(
            body["status"].get("lastVerified").is_none(),
            "multi-repo flat lastVerified is the controller's MIN, never a mover write"
        );
        assert!(
            body["status"].get("conditions").is_none(),
            "the conditions array is replace-on-merge; concurrent per-repo \
             writers must not touch it"
        );
    }

    /// THE race the entry-keyed design exists for: two per-repo verify movers
    /// finish concurrently and both merge-patch the policy status. A JSON merge
    /// patch (RFC 7396) merges OBJECT keys but replaces ARRAYS wholesale, so
    /// stamping a Vec would lose whichever repo patched first; the map keeps
    /// both. Simulated with a real RFC 7396 apply in either order.
    #[test]
    fn concurrent_per_repo_stamps_both_survive_merge_patching() {
        fn merge(target: &mut serde_json::Value, patch: &serde_json::Value) {
            // RFC 7396.
            if let (Some(t), Some(p)) = (target.as_object_mut(), patch.as_object()) {
                for (k, v) in p {
                    if v.is_null() {
                        t.remove(k);
                    } else if v.is_object() && t.get(k).is_some_and(|c| c.is_object()) {
                        merge(t.get_mut(k).unwrap(), v);
                    } else {
                        t.insert(k.clone(), v.clone());
                    }
                }
            } else {
                *target = patch.clone();
            }
        }
        let now = chrono::Utc::now();
        let a = verify_ok_body("quick", Some("Repository/backups/nas"), &now);
        let b = verify_ok_body("deep", Some("ClusterRepository/offsite"), &now);
        for (first, second) in [(&a, &b), (&b, &a)] {
            let mut status = serde_json::json!({});
            merge(&mut status, first);
            merge(&mut status, second);
            let stamps = &status["status"]["verificationStamps"];
            assert!(
                stamps["Repository/backups/nas"].is_string()
                    && stamps["ClusterRepository/offsite"].is_string(),
                "both concurrent stamps must survive in either order: {status}"
            );
        }
    }

    /// Wire-level `KubeStatusReporter` tests through a mock client
    /// (`tower::service_fn`, no cluster): the request PATH is the contract the
    /// mover RBAC authorizes, so it is asserted literally.
    mod reporter {
        use std::sync::{Arc, Mutex};

        use http::{Request, Response, StatusCode};
        use kube::client::Body;

        use super::super::KubeStatusReporter;
        use crate::workspec::TargetRef;

        fn target(kind: &str) -> TargetRef {
            TargetRef {
                api_version: kopiur_api::consts::API_VERSION.to_string(),
                kind: kind.to_string(),
                name: "plex".to_string(),
                namespace: "test-ns".to_string(),
            }
        }

        /// A mock client that logs every request path and answers with
        /// `status` + `body` (mirrors `controller::io::cached`'s harness).
        fn logging_client(
            log: Arc<Mutex<Vec<String>>>,
            status: StatusCode,
            body: serde_json::Value,
        ) -> kube::Client {
            let body = Arc::new(body);
            let svc = tower::service_fn(move |req: Request<Body>| {
                let log = log.clone();
                let body = body.clone();
                async move {
                    log.lock().unwrap().push(req.uri().path().to_string());
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .body(Body::from(serde_json::to_vec(&*body).unwrap()))
                            .unwrap(),
                    )
                }
            });
            kube::Client::new(svc, "test-ns")
        }

        fn not_found_body() -> serde_json::Value {
            serde_json::json!({
                "kind": "Status", "apiVersion": "v1", "status": "Failure",
                "reason": "NotFound", "code": 404,
            })
        }

        fn restore_with_resolved() -> serde_json::Value {
            serde_json::json!({
                "apiVersion": kopiur_api::consts::API_VERSION,
                "kind": "Restore",
                "metadata": { "name": "plex", "namespace": "test-ns", "uid": "uid-r" },
                "spec": {},
                "status": { "resolved": {
                    "resolution": "Snapshot",
                    "kopiaSnapshotID": "abc123",
                } },
            })
        }

        /// #401 regression guard: the read MUST go through the `/status`
        /// subresource — that is the resource the least-privilege mover role
        /// grants `get` on. The buggy code GETted the BASE resource
        /// (`.../restores/plex`), which the role does not grant, and 403'd on
        /// every in-Job restore resolution.
        #[tokio::test]
        async fn read_resolved_gets_the_status_subresource() {
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = logging_client(log.clone(), StatusCode::OK, restore_with_resolved());
            let reporter = KubeStatusReporter::from_client(client, &target("Restore"));
            let resolved = reporter
                .read_resolved()
                .await
                .expect("read succeeds against the mock");
            assert_eq!(
                log.lock().unwrap().as_slice(),
                [
                    "/apis/kopiur.home-operations.com/v1alpha1/namespaces/test-ns/restores/plex/status"
                ],
                "the read must target the status subresource, not the base resource (#401)"
            );
            assert_eq!(
                resolved
                    .expect("status.resolved present")
                    .kopia_snapshot_id
                    .as_deref(),
                Some("abc123"),
                "the pinned snapshot id round-trips"
            );
        }

        /// A deleted CR (404 NotFound) is "no pin", not an error — the same
        /// semantics `get_opt` gave the base-resource read.
        #[tokio::test]
        async fn read_resolved_maps_not_found_to_none() {
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = logging_client(log, StatusCode::NOT_FOUND, not_found_body());
            let reporter = KubeStatusReporter::from_client(client, &target("Restore"));
            let resolved = reporter
                .read_resolved()
                .await
                .expect("NotFound is not an error");
            assert!(resolved.is_none(), "a deleted CR has no pinned resolution");
        }

        /// A non-NotFound failure surfaces as `StatusRead` — naming the GET,
        /// not a PATCH (#401's log line said "failed to PATCH" for a rejected
        /// GET, which sent the reporter debugging the wrong call).
        #[tokio::test]
        async fn read_resolved_failure_names_the_read_not_a_patch() {
            let log = Arc::new(Mutex::new(Vec::new()));
            let forbidden = serde_json::json!({
                "kind": "Status", "apiVersion": "v1", "status": "Failure",
                "reason": "Forbidden", "code": 403,
            });
            let client = logging_client(log, StatusCode::FORBIDDEN, forbidden);
            let reporter = KubeStatusReporter::from_client(client, &target("Restore"));
            let err = reporter
                .read_resolved()
                .await
                .expect_err("403 is a real error");
            let msg = err.to_string();
            assert!(
                msg.starts_with("failed to read the status of Restore test-ns/plex"),
                "the message must name the read: {msg}"
            );
            assert!(
                !msg.contains("PATCH"),
                "a rejected GET must not be reported as a PATCH: {msg}"
            );
            // Environmental, not a kopia failure: Unknown and not retryable.
            assert_eq!(err.kopia_class(), kopiur_kopia::KopiaErrorClass::Unknown);
            assert!(!err.retry_recommended());
        }

        /// ClusterRepository is cluster-scoped: its status path must not carry
        /// a `/namespaces/` segment (a namespaced path 404s — the latent trap
        /// found while fixing #401; dead code today, guarded anyway).
        #[tokio::test]
        async fn cluster_scoped_target_builds_a_cluster_path() {
            let log = Arc::new(Mutex::new(Vec::new()));
            let client = logging_client(log.clone(), StatusCode::NOT_FOUND, not_found_body());
            let reporter = KubeStatusReporter::from_client(client, &target("ClusterRepository"));
            let _ = reporter.read_resolved().await;
            assert_eq!(
                log.lock().unwrap().as_slice(),
                ["/apis/kopiur.home-operations.com/v1alpha1/clusterrepositories/plex/status"],
                "cluster-scoped kinds must not use a namespaced path"
            );
        }
    }
}
