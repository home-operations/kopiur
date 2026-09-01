//! Repository index-blob health (ADR-0005 §13).
//!
//! kopia's content index is a set of "index blobs" that periodic maintenance
//! compacts. When maintenance stops keeping up — most often because a stale
//! maintenance-lease owner makes every run yield (see
//! `kopiur_mover::workspec::maintenance_restamp_target`) — the index-blob count
//! climbs unbounded and kopia eventually warns "Found too many index blobs (N)",
//! after which backups degrade.
//!
//! The bootstrap mover (and the in-process filesystem path) report the count via
//! `BootstrapResult.index_blob_count`. This module turns that count into a
//! non-blocking `IndexBlobHealth` condition + a one-shot Warning event when the
//! count crosses `spec.health.indexBlobWarnThreshold`. It is **informational**:
//! the repository stays `Ready`, so GitOps health gates are not tripped — too
//! many index blobs is a degradation, not an outage.

use chrono::{DateTime, Utc};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;

use kopiur_api::RepositoryPhase;
use kopiur_api::repository::{ProbeOnFailure, RepositoryHealthStatus};

use crate::consts::{
    BACKEND_REACHABLE_CONDITION, BACKEND_REACHABLE_REASON, BACKEND_UNREACHABLE_REASON,
    BOOTSTRAP_DEADLINE_EXCEEDED_REASON, CHECK_BACKEND_ACTION, INDEX_BLOB_HEALTH_CONDITION,
    PROBE_DEADLINE_EXCEEDED_REASON, RAISE_BOOTSTRAP_DEADLINE_ACTION,
    REPOSITORY_BOOTSTRAPPED_CONDITION, REPOSITORY_VANISHED_REASON, VERIFY_BACKEND_ACTION,
};
use crate::io;

/// Machine reason on the `IndexBlobHealth` condition (and the Warning event) when
/// the count is over threshold.
pub const TOO_MANY_INDEX_BLOBS_REASON: &str = "TooManyIndexBlobs";
/// Remediation `action` carried on the Warning event.
pub const ENSURE_MAINTENANCE_ACTION: &str = "EnsureMaintenance";

/// Event reason when `kopia repository set-parameters` was asked for and did not apply
/// (#258). The apply is best-effort by design — a bad parameter must not fail an otherwise
/// healthy repository — so this event is how the user finds out at all; without it the only
/// evidence is a `status.parameters.epoch` that quietly disagrees with `spec`.
pub const EPOCH_PARAMETERS_NOT_APPLIED_REASON: &str = "EpochParametersNotApplied";
/// Remediation label for [`EPOCH_PARAMETERS_NOT_APPLIED_REASON`].
pub const FIX_EPOCH_PARAMETERS_ACTION: &str = "FixEpochParameters";

/// Machine reason when the count is within threshold.
pub const INDEX_BLOBS_HEALTHY_REASON: &str = "Healthy";

/// The classification of an index-blob count against its threshold. A closed enum
/// so callers `match` exhaustively (CLAUDE.md "type-safety end-to-end").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexBlobHealth {
    /// Count is within the configured threshold (or the warning is disabled).
    Healthy,
    /// Count exceeds the threshold — maintenance isn't compacting fast enough.
    TooMany {
        /// Observed index-blob count.
        count: i64,
        /// The configured threshold it exceeded.
        threshold: i64,
    },
}

/// Classify `count` against `threshold`. A `threshold` of `0` (the documented
/// disable sentinel) — or any non-positive value — means "never warn". Otherwise
/// the count must be strictly **above** the threshold to be unhealthy (the
/// threshold is "the count above which we warn").
///
/// ```
/// use kopiur_controller::health::{classify_index_blob_health, IndexBlobHealth};
///
/// assert_eq!(classify_index_blob_health(500, 1000), IndexBlobHealth::Healthy);
/// assert_eq!(classify_index_blob_health(1000, 1000), IndexBlobHealth::Healthy); // not strictly above
/// assert_eq!(
///     classify_index_blob_health(1448, 1000),
///     IndexBlobHealth::TooMany { count: 1448, threshold: 1000 }
/// );
/// // 0 disables the warning entirely, even for a huge count.
/// assert_eq!(classify_index_blob_health(99999, 0), IndexBlobHealth::Healthy);
/// ```
pub fn classify_index_blob_health(count: i64, threshold: i64) -> IndexBlobHealth {
    if threshold > 0 && count > threshold {
        IndexBlobHealth::TooMany { count, threshold }
    } else {
        IndexBlobHealth::Healthy
    }
}

/// The outcome of folding the index-blob count into a repository's conditions:
/// the updated condition array (always — to fold into the next status patch) and,
/// when the repository *transitioned* into the unhealthy state on this reconcile,
/// the `(reason, action, message)` of the Warning event to publish.
pub struct IndexBlobHealthUpdate {
    /// Conditions with `IndexBlobHealth` upserted in place (order-stable).
    pub conditions: Vec<Condition>,
    /// `Some` only on the transition into unhealthy, so the event fires once per
    /// episode rather than on every reconcile.
    pub event: Option<IndexBlobWarning>,
}

/// A Warning event to publish: machine `reason`, remediation `action`, human `message`.
pub struct IndexBlobWarning {
    /// Machine-readable reason (e.g. `TooManyIndexBlobs`).
    pub reason: &'static str,
    /// Remediation hint carried as the event `action`.
    pub action: &'static str,
    /// Human-readable message for `kubectl describe`.
    pub message: String,
}

/// Reconcile the `IndexBlobHealth` condition for a count of `count` index blobs
/// against `threshold`, starting from the repository's `existing` conditions.
///
/// Pure (no I/O), so it is unit-tested directly and shared by the namespaced
/// `Repository` and the `ClusterRepository` reconcilers. The caller folds
/// [`IndexBlobHealthUpdate::conditions`] into its status patch and, if
/// [`IndexBlobHealthUpdate::event`] is `Some`, publishes it via
/// [`io::publish_warning_event`].
pub fn reconcile_index_blob_health(
    existing: &[Condition],
    count: i64,
    threshold: i64,
    generation: Option<i64>,
) -> IndexBlobHealthUpdate {
    // Was the repository already flagged unhealthy? Used to fire the event only on
    // the transition, not on every requeue while it stays unhealthy.
    let prior_unhealthy = existing
        .iter()
        .find(|c| c.type_ == INDEX_BLOB_HEALTH_CONDITION)
        .is_some_and(|c| c.status == "False");

    match classify_index_blob_health(count, threshold) {
        IndexBlobHealth::TooMany { count, threshold } => {
            // Three remedies, in the order they are usually the answer. The epoch one is
            // last but is the fix when maintenance is demonstrably running and the count
            // still will not fall: kopia cannot compact an index blob until its epoch
            // closes, an epoch cannot close before `minDuration` (24h by default) no matter
            // how many blobs pile up, and compaction then trails two epochs behind. A fleet
            // producing tens of blobs an hour is therefore held at thousands of uncompacted
            // blobs by the gate alone — no maintenance schedule can help, which is exactly
            // the dead end #258 was reported from.
            let message = format!(
                "repository has {count} content-index blobs (threshold {threshold}); maintenance \
                 is not compacting them. Fix: ensure maintenance runs — if stuck on a stale \
                 lease, set spec.maintenance.takeoverPolicy: Force once; if it IS running, the \
                 epoch gate is usually why — lower spec.parameters.epoch.minDuration (default \
                 24h, e.g. 6h) so blobs compact. Raise spec.health.indexBlobWarnThreshold \
                 (or 0) to silence."
            );
            let conditions = io::upsert_condition(
                existing,
                INDEX_BLOB_HEALTH_CONDITION,
                false,
                TOO_MANY_INDEX_BLOBS_REASON,
                &message,
                generation,
            );
            let event = (!prior_unhealthy).then_some(IndexBlobWarning {
                reason: TOO_MANY_INDEX_BLOBS_REASON,
                action: ENSURE_MAINTENANCE_ACTION,
                message,
            });
            IndexBlobHealthUpdate { conditions, event }
        }
        IndexBlobHealth::Healthy => {
            let conditions = io::upsert_condition(
                existing,
                INDEX_BLOB_HEALTH_CONDITION,
                true,
                INDEX_BLOBS_HEALTHY_REASON,
                "content-index blob count is within the configured threshold",
                generation,
            );
            IndexBlobHealthUpdate {
                conditions,
                event: None,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Backend health probe (`spec.health.probe`) — default-on since #345.
//
// Part A (always-on safety invariant) + Part B (the periodic probe).
// A vanished/unreachable backend is surfaced as the `BackendReachable`
// condition + a Warning event; what happens past `failureThreshold` is the
// spec's `onFailure` policy (default `Degrade` — the circuit breaker moves the
// repository to `Degraded` and pauses backups until a re-connect succeeds;
// `Alert` keeps the pre-#345 alert-only contract). NEVER an auto-recreate,
// under either policy.
// ---------------------------------------------------------------------------

/// **Part A — the data-safety invariant.** kopiur auto-creates a kopia
/// repository ONLY on the very first bootstrap. Once a repository has reached
/// `Ready` it carries a pinned `status.uniqueId` forever, so a later connect
/// failure (a wiped or unreachable backend) must NEVER be "fixed" by silently
/// creating a fresh empty repository over it — that destroys restorability.
/// Re-creation of a once-good repository is a deliberate human action.
///
/// Returns whether `create` may be attempted: the spec opted in (`create.enabled`)
/// AND this repository was never successfully bootstrapped (`uniqueId` unset). Used
/// at every create site (the in-process bare-path connect, the mover work-spec
/// builder, and the no-Job re-create path) in both reconcilers.
///
/// ```
/// use kopiur_controller::health::auto_create_allowed;
/// assert!(auto_create_allowed(true, None));            // first bootstrap, opted in
/// assert!(!auto_create_allowed(true, Some("abc123"))); // once-Ready: never recreate
/// assert!(!auto_create_allowed(false, None));          // not opted in
/// ```
pub fn auto_create_allowed(spec_create_enabled: bool, unique_id: Option<&str>) -> bool {
    spec_create_enabled && unique_id.is_none()
}

/// Whether a backend health probe is due now: the probe is enabled, the
/// repository was previously bootstrapped (`uniqueId` set — a probe of a
/// never-bootstrapped repo is meaningless), and `last_probe_at + interval <= now`
/// (or it has never probed). Keyed on `bootstrapped_before`, NOT on `phase ==
/// Ready`, so the probe keeps firing even after it has raised an alert.
///
/// Reuses [`crate::catalog::refresh_due`] for the timer so the probe and the
/// catalog re-scan share identical, deterministic due-logic.
pub fn health_probe_due(
    bootstrapped_before: bool,
    enabled: bool,
    last_probe_at: Option<&str>,
    interval: std::time::Duration,
    now: DateTime<Utc>,
) -> bool {
    enabled && bootstrapped_before && crate::catalog::refresh_due(last_probe_at, interval, now)
}

/// Extra headroom over the bootstrap Job's own deadline before a stamped
/// `probeAttemptAt` is treated as abandoned. Covers the gap between the Job going
/// terminal and the controller finalizing it (a restart, a requeue backoff).
pub const PROBE_ATTEMPT_GRACE_SECS: i64 = 60;

/// The bootstrap Job's `activeDeadlineSeconds`: the user's override, else
/// [`crate::consts::BOOTSTRAP_JOB_DEADLINE_SECS`]. One definition shared by the Job
/// builder and [`probe_attempt_timeout`], so the launch bound and the gate that
/// waits on it can never disagree.
pub fn bootstrap_deadline_seconds(active_deadline_seconds: Option<i64>) -> i64 {
    active_deadline_seconds.unwrap_or(crate::consts::BOOTSTRAP_JOB_DEADLINE_SECS)
}

/// How long a stamped `probeAttemptAt` still means "a probe Job is in flight".
///
/// Derived from the Job's own deadline rather than the probe interval: the
/// interval floor is 30s (and the e2e uses exactly that), so a slow backend would
/// be declared abandoned mid-flight. A bootstrap Job cannot outlive its
/// `activeDeadlineSeconds`, so that plus a grace window is the true upper bound.
pub fn probe_attempt_timeout(active_deadline_seconds: i64) -> std::time::Duration {
    std::time::Duration::from_secs(
        active_deadline_seconds
            .saturating_add(PROBE_ATTEMPT_GRACE_SECS)
            .max(1) as u64,
    )
}

/// Whether a probe Job launched at `attempt_at` is still plausibly in flight.
///
/// **Fails OPEN**: an absent or unparseable stamp reads as "not pending", so a
/// malformed value can never wedge the probe forever. This controller is the only
/// writer of the field, mirroring [`crate::catalog::scan_requested_due`]'s
/// reasoning.
pub fn probe_attempt_pending(
    attempt_at: Option<&str>,
    timeout: std::time::Duration,
    now: DateTime<Utc>,
) -> bool {
    let Some(raw) = attempt_at else {
        return false;
    };
    let Ok(started) = DateTime::parse_from_rfc3339(raw) else {
        return false;
    };
    let Ok(timeout) = chrono::Duration::from_std(timeout) else {
        return false;
    };
    started.with_timezone(&Utc) + timeout > now
}

/// What the backend health probe wants from this reconcile pass.
///
/// A closed enum so both reconcilers stay symmetric and a new variant cannot
/// compile until every arm accounts for it (CLAUDE.md "type-safety end-to-end").
/// It replaces the bare `probe_due` bool, whose single value had to serve two
/// incompatible questions — "should I launch a probe?" and "is this finished Job
/// my probe's result?" — which is precisely how #273 happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAction {
    /// Nothing to do: disabled, never bootstrapped, not `Ready`, or not yet due.
    Idle,
    /// A probe is due and none is in flight — (re)launch the bootstrap Job.
    Launch,
    /// A probe Job was launched and its result has not been finalized yet. The
    /// finished Job MUST be allowed through to `finalize_*` — recycling it here is
    /// the #273 livelock.
    AwaitingResult,
}

impl ProbeAction {
    /// Whether this pass should launch (or recycle-then-relaunch) a probe Job.
    pub fn launches(self) -> bool {
        match self {
            Self::Launch => true,
            Self::Idle | Self::AwaitingResult => false,
        }
    }

    /// Whether a finished bootstrap Job observed on this pass is *this probe's*
    /// result, and must therefore be finalized as a probe rather than recycled.
    pub fn awaits_result(self) -> bool {
        match self {
            Self::AwaitingResult => true,
            Self::Idle | Self::Launch => false,
        }
    }
}

/// Decide what the probe wants from this pass.
///
/// **The #273 fix, and the mirror image of [`crate::catalog::scan_requested_pending`].**
/// That doc comment warns against using the *rate-limited launch* predicate at
/// *finalize* time. This is the converse hazard: `lastProbeAt` is a *finalize-side*
/// stamp, and gating the *launch-side* recycle on it alone means the gate deletes
/// every Job whose completion would have cleared it — so `finalize_*` (the only
/// writer of `lastProbeAt`) is unreachable and the probe recycles its Job forever.
/// The launch-side stamp `probeAttemptAt` is what breaks that cycle.
///
/// `phase_is_ready` is load-bearing twice over. It stops a probe from racing a real
/// (re-)bootstrap — mirroring [`crate::catalog::bootstrap_recycle_due`]'s own
/// `if !phase_is_ready { return false }` guard, which the probe arm was missing —
/// and it stops a terminally-`Failed` repository's lingering Job from being re-read
/// as a probe, which would flip it back to `Ready` (see `finalize_probe_failure`).
/// It does NOT weaken [`health_probe_due`]'s deliberate "keyed on
/// `bootstrapped_before`, not `phase == Ready`" property: a repository that has
/// raised an alert *stays* `Ready` by design, so it keeps probing.
#[allow(clippy::too_many_arguments)]
pub fn probe_action(
    phase_is_ready: bool,
    bootstrapped_before: bool,
    enabled: bool,
    last_probe_at: Option<&str>,
    probe_attempt_at: Option<&str>,
    interval: std::time::Duration,
    attempt_timeout: std::time::Duration,
    now: DateTime<Utc>,
) -> ProbeAction {
    if !(enabled && bootstrapped_before && phase_is_ready) {
        return ProbeAction::Idle;
    }
    if probe_attempt_pending(probe_attempt_at, attempt_timeout, now) {
        return ProbeAction::AwaitingResult;
    }
    if health_probe_due(bootstrapped_before, enabled, last_probe_at, interval, now) {
        return ProbeAction::Launch;
    }
    ProbeAction::Idle
}

/// How a failing probe is classified — the load-bearing distinction the user
/// demanded before anything destructive is ever even *suggested*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailureKind {
    /// Backend reachable, kopia format blob absent (`RepositoryNotInitialized`) —
    /// a candidate *vanished* repository. Still only an alert.
    Vanished,
    /// Backend unreachable, mount/path missing, or auth/lock failed — NOT a
    /// confirmed wipe. kopiur never treats it as one.
    Unreachable,
    /// The probe/bootstrap Job was killed by its `activeDeadlineSeconds` before
    /// kopia could report a verdict (issue #414): the backend may be reachable
    /// but SLOW — a cold-cache `kopia repository connect` scales with
    /// index-blob count. Counts toward the breaker threshold (a repository
    /// that cannot be confirmed healthy pauses backups/replication), but
    /// maintenance keeps running: index compaction is the cure
    /// (`maintenance_may_proceed`), and the retry deadline escalates.
    TimedOut,
}

impl ProbeFailureKind {
    /// The stable metric-label value (`vanished` / `unreachable` /
    /// `timed_out`), shared by the `kopiur_repository_health_probe_failures`
    /// `outcome` label and the `kopiur_repository_breaker_trips` `probe_kind`
    /// label so dashboards join the two on identical values.
    pub fn label(self) -> &'static str {
        match self {
            ProbeFailureKind::Vanished => "vanished",
            ProbeFailureKind::Unreachable => "unreachable",
            ProbeFailureKind::TimedOut => "timed_out",
        }
    }
}

/// Classify a [`crate::io::BootstrapFailure`] as the unified backend sensor's
/// failure kind — the ONE place the mapping lives (it was four hand-copied
/// two-way collapses across the twins before #414). Exhaustive: only the
/// mover's genuine `RepositoryNotInitialized` sentinel may read as *vanished*
/// (a wrong "absent" here could, in a future auto-recreate, be catastrophic),
/// only a Job-deadline kill reads as *timed out*, and everything else is the
/// conservative *unreachable*.
pub fn probe_failure_kind(failure: &crate::io::BootstrapFailure) -> ProbeFailureKind {
    use crate::io::BootstrapFailure;
    match failure {
        BootstrapFailure::RepositoryNotInitialized => ProbeFailureKind::Vanished,
        BootstrapFailure::JobDeadlineExceeded { .. } => ProbeFailureKind::TimedOut,
        BootstrapFailure::Backend { .. }
        | BootstrapFailure::JobFailedWithoutResult { .. }
        | BootstrapFailure::Seed { .. }
        | BootstrapFailure::SeedMoverTooOld
        | BootstrapFailure::InternalInconsistency { .. } => ProbeFailureKind::Unreachable,
    }
}

/// A Warning event produced by the probe (machine `reason`, remediation `action`,
/// human `message`).
pub struct ProbeWarning {
    /// Machine-readable reason (e.g. `RepositoryVanished`).
    pub reason: &'static str,
    /// Remediation hint carried as the event `action`.
    pub action: &'static str,
    /// Human-readable message for `kubectl describe`.
    pub message: String,
}

/// The result of folding a probe outcome into a repository's health state: the
/// updated `conditions` array (always — to fold into the next status patch), the
/// new `health` status sub-object to pin, and (only on a transition) the Warning
/// event to publish.
pub struct ProbeUpdate {
    /// Conditions with `BackendReachable` upserted in place (order-stable).
    pub conditions: Vec<Condition>,
    /// The `status.health` sub-object to write (counters + timestamps).
    pub health: RepositoryHealthStatus,
    /// `Some` only when the alert state *transitions* (first crossing of the
    /// failure threshold, or a change of failure reason) — fired once per episode.
    pub event: Option<ProbeWarning>,
}

/// Fold a **successful** probe into the health state: `BackendReachable=True`,
/// stamp `lastProbeAt`/`lastHealthyAt`, and reset the failure debounce. Phase is
/// the caller's concern and stays `Ready`.
pub fn reconcile_probe_success(
    existing: &[Condition],
    now: &str,
    generation: Option<i64>,
) -> ProbeUpdate {
    let conditions = io::upsert_condition(
        existing,
        BACKEND_REACHABLE_CONDITION,
        true,
        BACKEND_REACHABLE_REASON,
        "the last health probe reached the backend and the repository is present",
        generation,
    );
    ProbeUpdate {
        conditions,
        health: RepositoryHealthStatus {
            last_probe_at: Some(now.to_string()),
            last_healthy_at: Some(now.to_string()),
            consecutive_probe_failures: None,
            first_failure_at: None,
            probe_attempt_at: None,
        },
        event: None,
    }
}

/// The `status.health` merge-patch fragment for a **successful** probe. Emits
/// explicit JSON `null` for the two failure-debounce fields so an RFC 7386 merge
/// patch actually CLEARS them: a `None` would be `skip_serializing_if`-elided and
/// leave the prior failing streak in the stored object, which re-fires the loud
/// alert on the very next single failure (defeating `failureThreshold`). Use this
/// instead of serializing [`reconcile_probe_success`]'s `health` directly.
pub fn probe_success_health_patch(now: &str) -> serde_json::Value {
    serde_json::json!({
        "lastProbeAt": now,
        "lastHealthyAt": now,
        "consecutiveProbeFailures": serde_json::Value::Null,
        "firstFailureAt": serde_json::Value::Null,
        // Retire the launch-side attempt stamp: this probe's result is now recorded,
        // so the next pass must read `Idle`/`Launch`, never `AwaitingResult` (#273).
        "probeAttemptAt": serde_json::Value::Null,
    })
}

/// The `status.health` merge-patch fragment for a **failing** probe.
///
/// [`reconcile_probe_failure`] returns a typed [`RepositoryHealthStatus`], and every
/// field is `skip_serializing_if = "Option::is_none"` — so serializing it directly
/// can never CLEAR `probeAttemptAt`; the `None` is simply elided and the stale stamp
/// survives in the stored object, leaving the repository stuck `AwaitingResult` until
/// the timeout and making the *next* finished Job read as a probe. Emits the explicit
/// RFC 7386 null the merge needs. Success twin: [`probe_success_health_patch`].
pub fn probe_failure_health_patch(health: &RepositoryHealthStatus) -> serde_json::Value {
    let mut value = serde_json::to_value(health).unwrap_or_else(|_| serde_json::json!({}));
    value["probeAttemptAt"] = serde_json::Value::Null;
    value
}

/// The `status.health` merge-patch fragment written when a bootstrap Job is
/// **launched**: stamps `probeAttemptAt` for a probe-style launch, or explicitly
/// clears it for any other launch.
///
/// Clearing is not optional. A strict relaunch (spec change, reverify) that merely
/// *left* a stale stamp alone would have its result re-read as a probe on the next
/// pass — fabricating `lastHealthyAt` from a connect no probe requested, and (worse)
/// routing a genuine bootstrap failure into `finalize_probe_failure`, which under
/// `onFailure: Alert` (or below the threshold) writes `phase: Ready`. Writing
/// `now | null` at the single launch site kills that whole class at the source.
pub fn probe_attempt_health_patch(launched_at: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "probeAttemptAt": match launched_at {
            Some(now) => serde_json::Value::String(now.to_string()),
            None => serde_json::Value::Null,
        },
    })
}

/// Whether the repository's `BackendReachable` condition is currently `True`.
/// An absent condition reads as *not* True — the unified success fold then
/// writes it once (converging to True), so absence never persists past one
/// successful finalize.
pub fn backend_reachable_true(conditions: &[Condition]) -> bool {
    conditions
        .iter()
        .find(|c| c.type_ == BACKEND_REACHABLE_CONDITION)
        .is_some_and(|c| c.status == "True")
}

/// Why [`success_fold`] decided a successful bootstrap finalize must write
/// probe-health state. A closed enum so tests (and log lines) can name the
/// exact trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuccessFoldReason {
    /// This finalize is a probe run's own result. Always written — a probe
    /// consumes (deletes) its Job exactly once, so this can never re-run on the
    /// same result.
    Probe,
    /// First successful strict finalize with no `lastProbeAt` on record: seed
    /// the stamp so `health_probe_due` (which fires when `lastProbeAt` is
    /// `None`) does not launch a redundant probe Job fleet-wide right after the
    /// default-on upgrade — the strict connect that just succeeded IS a fresh
    /// backend verdict.
    Seed,
    /// A failure streak is recorded (or the `BackendReachable` condition is not
    /// `True`): any successful strict connect is proof the backend is healthy
    /// again, so heal the stale state. This is what closes the breaker after a
    /// `Degraded` repository re-connects (M4).
    Heal,
}

/// The pieces a successful finalize folds into its status patch when
/// [`success_fold`] says a write is warranted.
pub struct SuccessFold {
    /// Why the write is happening (probe result / seeding / healing).
    pub reason: SuccessFoldReason,
    /// The `status.health` merge-patch fragment — [`probe_success_health_patch`],
    /// which stamps `lastProbeAt`/`lastHealthyAt` and emits the explicit RFC 7386
    /// nulls that clear the failure debounce and retire `probeAttemptAt`.
    pub health_patch: serde_json::Value,
}

/// Decide whether a **successful** bootstrap finalize (probe or strict) should
/// fold probe-success health state into its status patch — and refuse when it
/// would change nothing.
///
/// **The refusal is the load-bearing part.** The strict-bootstrap success arm
/// re-runs on EVERY reconcile while the finished bootstrap Job lingers (~1h
/// TTL), and its status write is guarded by `patch_status_if_changed`: the
/// steady-state pass must be a byte-stable no-op or the write bumps
/// `resourceVersion`, re-triggers the reconciler through its own primary
/// watch, and hot-loops. A naive "stamp `lastProbeAt: now` on every success"
/// would differ on every pass. So the fold fires only when:
///
/// * `probe_run` — a probe's own result; safe to stamp unconditionally because
///   the probe deletes its Job after finalizing (once-only), or
/// * `lastProbeAt` was never stamped — one-time seeding (see
///   [`SuccessFoldReason::Seed`]), or
/// * a failure streak / non-`True` `BackendReachable` needs healing (see
///   [`SuccessFoldReason::Heal`]).
///
/// In the steady state (strict re-read, seeded, healthy) it returns `None` and
/// the fold contributes NOTHING to the patch.
pub fn success_fold(
    probe_run: bool,
    health: Option<&RepositoryHealthStatus>,
    backend_reachable_true: bool,
    now: &str,
) -> Option<SuccessFold> {
    let reason = if probe_run {
        SuccessFoldReason::Probe
    } else if health.and_then(|h| h.last_probe_at.as_deref()).is_none() {
        SuccessFoldReason::Seed
    } else if health
        .and_then(|h| h.consecutive_probe_failures)
        .unwrap_or(0)
        > 0
        || health.and_then(|h| h.first_failure_at.as_deref()).is_some()
        || !backend_reachable_true
    {
        SuccessFoldReason::Heal
    } else {
        // Steady state: healthy, seeded, not a probe. Writing anything here
        // (even an identical value with a fresh timestamp) defeats the
        // byte-stable no-op the finalize arm depends on.
        return None;
    };
    Some(SuccessFold {
        reason,
        health_patch: probe_success_health_patch(now),
    })
}

/// Fold ONE backend failure into the unified sensor's `status.health` state:
/// streak = prior+1, `firstFailureAt` continues an active streak (fresh stamp
/// on a new episode), `lastProbeAt` = now — the anchor
/// [`strict_retry_holdoff`] measures the relaunch backoff from. Extracted from
/// [`reconcile_probe_failure`] (which delegates) so the result-less recycle
/// route can arm the SAME holdoff (#415: that route previously wrote no
/// health at all, so the anchor went stale and the gate failed open into a
/// flat ~2.5-minute relaunch metronome).
///
/// Widening note: this stamps `lastProbeAt` on repositories no probe ever ran
/// against. That is safe for `health_probe_due`'s timer because `probe_action`
/// requires `phase_is_ready` and every writer here also writes a non-Ready
/// phase; and it is exactly what the holdoff needs (a per-failure anchor).
/// `probe_attempt_at` is `None` here — callers MUST patch through
/// [`probe_failure_health_patch`], which emits the explicit JSON null a typed
/// `None` cannot express through a merge patch.
pub fn failure_streak_health(
    prior: Option<&RepositoryHealthStatus>,
    now: &str,
) -> RepositoryHealthStatus {
    let prior_failures = prior
        .and_then(|h| h.consecutive_probe_failures)
        .unwrap_or(0)
        .max(0);
    let first_failure_at = prior
        .and_then(|h| {
            if prior_failures > 0 {
                h.first_failure_at.clone()
            } else {
                None
            }
        })
        .unwrap_or_else(|| now.to_string());
    RepositoryHealthStatus {
        last_probe_at: Some(now.to_string()),
        last_healthy_at: prior.and_then(|h| h.last_healthy_at.clone()),
        consecutive_probe_failures: Some(prior_failures + 1),
        first_failure_at: Some(first_failure_at),
        probe_attempt_at: None,
    }
}

/// Fold a **failing** probe into the health state (via
/// [`failure_streak_health`]), applying the consecutive-failure debounce: the
/// loud `BackendReachable=False` condition (and its Warning event) is raised
/// only once `failure_threshold` consecutive failures have accrued, so a
/// single transient blip never alarms or nudges a destructive manual recreate.
///
/// **Phase is the caller's concern** and is mode-dependent since M4: the caller
/// feeds the post-fold streak into [`breaker_verdict`] — under `onFailure:
/// Alert` (or below the threshold) the phase stays `Ready` (alert-only, the
/// pre-#345 contract); under the default `Degrade` a threshold-crossing streak
/// opens the circuit breaker and the caller moves the phase to `Degraded`.
/// This fn also feeds the STRICT retry loop while the breaker is open (the
/// recycle-to-`Degraded` path in both `finalize_bootstrap` twins folds it), so
/// `consecutiveProbeFailures` keeps climbing across probe *and* strict connect
/// failures — one unified backend sensor.
///
/// The event fires on a *transition*: the first reconcile that crosses the
/// threshold, or one where the failure *reason* changes (e.g. `BackendUnreachable`
/// → `RepositoryVanished`), so an escalation is never silently swallowed.
pub fn reconcile_probe_failure(
    existing: &[Condition],
    prior: Option<&RepositoryHealthStatus>,
    kind: ProbeFailureKind,
    failure_threshold: i64,
    now: &str,
    generation: Option<i64>,
) -> ProbeUpdate {
    let health = failure_streak_health(prior, now);
    let consecutive = health.consecutive_probe_failures.unwrap_or(1);

    let threshold = failure_threshold.max(1);
    if consecutive < threshold {
        // Debounce window: not yet confident. Leave conditions untouched (the prior
        // BackendReachable state stands) and emit no event.
        return ProbeUpdate {
            conditions: existing.to_vec(),
            health,
            event: None,
        };
    }

    let (reason, action, message): (&'static str, &'static str, String) = match kind {
        ProbeFailureKind::Vanished => (
            REPOSITORY_VANISHED_REASON,
            VERIFY_BACKEND_ACTION,
            format!(
                "kopia repository VANISHED: the backend is reachable but the format blob is \
                 absent ({consecutive} consecutive failing probes). Data blobs may still remain, \
                 so re-creating would orphan them and destroy restorability — kopiur will NOT \
                 auto-recreate. Fix: verify the backend is truly empty (and no other \
                 Repository/ClusterRepository points at it) before any deliberate re-create."
            ),
        ),
        ProbeFailureKind::Unreachable => (
            BACKEND_UNREACHABLE_REASON,
            CHECK_BACKEND_ACTION,
            format!(
                "repository backend not confirmed healthy ({consecutive} consecutive failing \
                 probes): unreachable, the path/mount is missing, or credentials/lock failed. \
                 This is NOT treated as a wipe and kopiur never auto-recreates. Fix: check the \
                 backend, credentials, and any mounted volume (see the Ready condition for what \
                 kopiur is doing)."
            ),
        ),
        ProbeFailureKind::TimedOut => (
            PROBE_DEADLINE_EXCEEDED_REASON,
            RAISE_BOOTSTRAP_DEADLINE_ACTION,
            format!(
                "backend health probe killed by its activeDeadlineSeconds ({consecutive} \
                 consecutive deadline-killed probes) — the backend may be reachable but slow: a \
                 cold cache over a large index makes connect exceed the deadline. This is NOT \
                 evidence of an outage or wipe, and kopiur never auto-recreates. Fix: raise \
                 spec.bootstrap.failurePolicy.activeDeadlineSeconds; maintenance shrinks connect \
                 time and keeps running."
            ),
        ),
    };

    // Fire on a transition: prior was not already `False` with this same reason.
    let prior_false_reason = existing
        .iter()
        .find(|c| c.type_ == BACKEND_REACHABLE_CONDITION)
        .filter(|c| c.status == "False")
        .map(|c| c.reason.as_str());
    let event = (prior_false_reason != Some(reason)).then_some(ProbeWarning {
        reason,
        action,
        message: message.clone(),
    });
    let conditions = io::upsert_condition(
        existing,
        BACKEND_REACHABLE_CONDITION,
        false,
        reason,
        &message,
        generation,
    );
    ProbeUpdate {
        conditions,
        health,
        event,
    }
}

// ---------------------------------------------------------------------------
// The repository circuit breaker (#345 M4).
//
// The probe (and the strict retry loop) is the sensor; `breaker_verdict` is the
// switch. Open = phase `Degraded`, which the backup/replication consumer gates
// read as "paused". Maintenance is deliberately EXEMPT since #413
// (`maintenance_may_proceed`): it keeps running unless the backend is
// confirmed unreachable/vanished, because index compaction is often the cure.
// Closed again by any successful strict connect via `success_fold`'s Heal case.
// ---------------------------------------------------------------------------

/// What a threshold-crossing check decides for the repository phase. A closed
/// two-state enum so both reconcilers `match` exhaustively (CLAUDE.md
/// "type-safety end-to-end") and a future variant cannot compile unhandled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerVerdict {
    /// Keep the phase `Ready`: the streak is below the threshold, or the spec
    /// chose `onFailure: Alert` (alert-only, the pre-#345 contract).
    StayReady,
    /// Open the circuit breaker: move the phase to `Degraded`, pausing backups,
    /// maintenance, and replication until a connect succeeds (recovery is
    /// automatic via the strict retry loop + `success_fold`).
    Open,
}

/// Decide whether a failing backend sensor opens the circuit breaker: `Open`
/// iff `consecutive >= threshold` (clamped to at least 1, mirroring
/// [`reconcile_probe_failure`]) AND the effective policy is
/// [`ProbeOnFailure::Degrade`]. Exhaustive over [`ProbeOnFailure`].
///
/// Deliberately does NOT take a [`ProbeFailureKind`]: BOTH kinds open the
/// breaker. `Unreachable` is the obvious outage; a `Vanished` verdict (backend
/// reachable, kopia format blob absent) ALSO makes every backup doomed, so
/// pausing is equally right — and the breaker's own strict re-check escalates a
/// *real* wipe to terminal `Failed` anyway: while `Degraded`, the strict retry
/// bootstrap runs with auto-create forbidden (pinned `uniqueId`, see
/// [`auto_create_allowed`]), so a truly absent repository comes back as the
/// `RepositoryNotInitialized` verdict, which
/// [`BootstrapFailure::retryable_outage_for_bootstrapped`](crate::io::BootstrapFailure::retryable_outage_for_bootstrapped)
/// refuses to recycle — parking the repository visibly at `Failed` instead of
/// looping `Degraded` forever over a backend that needs a human.
pub fn breaker_verdict(
    consecutive: i64,
    threshold: i64,
    on_failure: ProbeOnFailure,
) -> BreakerVerdict {
    match on_failure {
        ProbeOnFailure::Alert => BreakerVerdict::StayReady,
        ProbeOnFailure::Degrade => {
            if consecutive >= threshold.max(1) {
                BreakerVerdict::Open
            } else {
                BreakerVerdict::StayReady
            }
        }
    }
}

/// The machine `reason` carried by the kstatus `Ready` condition (and the
/// breaker-trip Warning event) while the breaker is open — reuses the
/// `BackendReachable` reason vocabulary so status and event agree on the cause.
pub fn breaker_reason(kind: ProbeFailureKind) -> &'static str {
    match kind {
        ProbeFailureKind::Vanished => REPOSITORY_VANISHED_REASON,
        ProbeFailureKind::Unreachable => BACKEND_UNREACHABLE_REASON,
        ProbeFailureKind::TimedOut => PROBE_DEADLINE_EXCEEDED_REASON,
    }
}

/// The remediation `action` for the breaker-trip Warning event, matching the
/// probe alert's own kind→action mapping.
pub fn breaker_action(kind: ProbeFailureKind) -> &'static str {
    match kind {
        ProbeFailureKind::Vanished => VERIFY_BACKEND_ACTION,
        ProbeFailureKind::Unreachable => CHECK_BACKEND_ACTION,
        ProbeFailureKind::TimedOut => RAISE_BOOTSTRAP_DEADLINE_ACTION,
    }
}

/// The BYTE-STABLE condition message written while the breaker is open: names
/// what is paused and that recovery is automatic, with no volatile detail (the
/// stderr tail rides the Warning event only), so the guarded status write is a
/// true no-op across repeated identical failures.
pub fn breaker_open_message(kind: ProbeFailureKind) -> &'static str {
    match kind {
        ProbeFailureKind::Vanished => {
            "the backend is reachable but the kopia repository is absent — the circuit breaker \
             is open: backups, maintenance, and replication are paused until a connect succeeds; \
             retrying with backoff (recovery is automatic; kopiur never auto-recreates)"
        }
        ProbeFailureKind::Unreachable => {
            "the repository backend is unreachable — the circuit breaker is open: backups, \
             maintenance, and replication are paused until a connect succeeds; retrying with \
             backoff (recovery is automatic)"
        }
        ProbeFailureKind::TimedOut => {
            "the repository connect keeps exceeding its bootstrap deadline — the circuit breaker \
             is open: backups and replication are paused until a connect succeeds; maintenance \
             still runs (index compaction shrinks connect time); retrying with backoff and a \
             progressively longer deadline (recovery is automatic). Raise \
             spec.bootstrap.failurePolicy.activeDeadlineSeconds if the backend is just slow"
        }
    }
}

/// Everything a probe-failure finalize writes that depends on the breaker
/// verdict — computed purely (and unit-tested once for both repository kinds)
/// so the finalize IO fns stay thin callers, the house pattern.
pub struct ProbeFailurePhase {
    /// Whether this verdict OPENS the breaker: drives the trip event/metric
    /// (fired on the transition only) and the short requeue into the strict
    /// retry loop.
    pub opened: bool,
    /// The typed phase, for [`crate::io::ready_outcome_for_phase`].
    pub phase: RepositoryPhase,
    /// The phase as written into the status merge patch.
    pub phase_str: &'static str,
    /// The kstatus `Ready` condition reason.
    pub ready_reason: &'static str,
    /// The kstatus `Ready` condition message — byte-stable (volatile detail
    /// rides the Event only), so the guarded status write stays a no-op across
    /// repeated identical failures.
    pub ready_message: &'static str,
    /// The requeue after finalizing: the caller's steady probe cadence while
    /// the phase stays `Ready`, or a short hop into the strict retry loop
    /// (whose cadence [`strict_retry_holdoff`] then governs) once opened.
    pub requeue: std::time::Duration,
}

/// Map a [`BreakerVerdict`] + [`ProbeFailureKind`] to the phase/conditions/
/// requeue a probe-failure finalize writes. `StayReady` keeps writing
/// `phase: Ready` explicitly — so under `onFailure: Alert` (or below the
/// threshold) a stray `Initializing`/`Degraded` from an earlier path still
/// self-heals, the pre-#345 contract. `Open` writes `Degraded` with the
/// byte-stable breaker message; it must NOT self-heal back — only a successful
/// connect closes the breaker (via [`success_fold`]).
pub fn probe_failure_phase(
    verdict: BreakerVerdict,
    kind: ProbeFailureKind,
    steady_requeue: std::time::Duration,
) -> ProbeFailurePhase {
    match verdict {
        BreakerVerdict::StayReady => ProbeFailurePhase {
            opened: false,
            phase: RepositoryPhase::Ready,
            phase_str: "Ready",
            ready_reason: "Bootstrapped",
            ready_message: "repository connected; a health probe is failing (see \
                            BackendReachable), backups continue",
            requeue: steady_requeue,
        },
        BreakerVerdict::Open => ProbeFailurePhase {
            opened: true,
            phase: RepositoryPhase::Degraded,
            phase_str: "Degraded",
            ready_reason: breaker_reason(kind),
            ready_message: breaker_open_message(kind),
            requeue: std::time::Duration::from_secs(5),
        },
    }
}

/// The `status.phase` a bootstrap-Job LAUNCH (or a running-Job poll) writes for
/// a repository that is not yet `Ready`.
///
/// A `Degraded` repository keeps `Degraded` while its strict retry Job runs:
/// the breaker retries on a cycle, and flapping `Degraded`↔`Initializing` every
/// cycle would break any phase-keyed alert or gauge (audit 3d) — and briefly
/// misreport a paused repository as making first-bootstrap progress. Every
/// other pre-`Ready` state (first bootstrap, a spec-change re-bootstrap of a
/// `Ready`/`Failed` repo, a never-reconciled `Pending`) shows `Initializing` as
/// before. Exhaustive over [`RepositoryPhase`] — no `_ =>` — so a new phase
/// must choose.
pub fn launch_phase(prior: Option<&RepositoryPhase>) -> &'static str {
    match prior {
        Some(RepositoryPhase::Degraded) => "Degraded",
        Some(
            RepositoryPhase::Pending
            | RepositoryPhase::Initializing
            | RepositoryPhase::Ready
            | RepositoryPhase::Failed,
        )
        | None => "Initializing",
        // A phase this build cannot interpret is not a known-paused `Degraded`
        // repository, so the launch reports plain first-bootstrap progress
        // rather than inventing a breaker state it cannot verify.
        Some(RepositoryPhase::Unknown(_)) => "Initializing",
    }
}

/// The single cap on the strict-retry relaunch backoff (and, through
/// [`strict_retry_holdoff`], the launch gate), shared so the finalize requeue
/// and the gate can never disagree. 1800s (#415, matching the default probe
/// interval): against a paid-per-request object store every relaunch is a
/// cold-cache `kopia repository connect` billed per attempt, so a doomed loop
/// must decay to ~2 attempts/hour — while a healed backend is still noticed
/// within 30 minutes worst case.
pub const STRICT_RETRY_BACKOFF_CAP_SECS: u64 = 1800;

/// Requeue for the strict recycle-retry loop while the backend is unavailable:
/// exponential from 120s, doubling per consecutive failure, capped at
/// [`STRICT_RETRY_BACKOFF_CAP_SECS`] — `(120, 240, 480, 960, 1800, 1800, …)`
/// for inputs `0, 1, 2, 3, 4, …` (negative/zero input → 120s). Bounds
/// multi-day-outage Job churn (~48 Jobs/day worst case instead of 720) while
/// keeping worst-case recovery detection ≤ 30m. The caller passes the number
/// of failures *before* the retry being scheduled (post-fold streak minus
/// one), so the first retry after a fresh failure waits the base 120s.
pub fn strict_retry_backoff(consecutive_failures: i64) -> std::time::Duration {
    const BASE_SECS: u64 = 120;
    // 120 << 4 = 1920 already exceeds the cap, so clamping the exponent at 4
    // keeps the shift small and overflow-free for any i64 input (and any
    // future cap value only needs a clamp bump the test derives).
    let exponent = consecutive_failures.clamp(0, 4) as u32;
    std::time::Duration::from_secs((BASE_SECS << exponent).min(STRICT_RETRY_BACKOFF_CAP_SECS))
}

/// How long a `Degraded` repository must still WAIT before relaunching its
/// strict retry bootstrap Job: `Some(remaining)` to hold (requeue that long),
/// `None` to launch now.
///
/// This launch-side gate is what makes [`strict_retry_backoff`] real. The
/// finalize path's backoff *requeue* alone cannot bound Job churn: the status
/// patch that records the failure (streak++) re-triggers the reconciler through
/// its own primary watch immediately, and `bootstrap_create_due` is
/// unconditionally true for any non-`Ready` phase — so without this gate every
/// retry cycle relaunches the Job the moment the previous one is finalized,
/// regardless of the requeue. Same launch-stamp discipline as
/// [`crate::catalog::scan_requested_due`], keyed on `status.health.lastProbeAt`
/// (which [`failure_streak_health`] stamps on every failure fold — probe,
/// strict, or the result-less recycle route since #415).
///
/// **Fails OPEN** on every edge: not `Degraded`, no recorded failure streak, an
/// absent or unparseable stamp, or an elapsed backoff all mean "launch now" —
/// a malformed value can never wedge the retry loop (this controller is the
/// only writer of the field). A spec change bypasses the gate at the call site
/// (the user changed config and expects an immediate retry).
pub fn strict_retry_holdoff(
    phase_is_degraded: bool,
    consecutive_failures: i64,
    last_failure_at: Option<&str>,
    now: DateTime<Utc>,
) -> Option<std::time::Duration> {
    if !phase_is_degraded || consecutive_failures <= 0 {
        return None;
    }
    let last = DateTime::parse_from_rfc3339(last_failure_at?).ok()?;
    let backoff = strict_retry_backoff(consecutive_failures - 1);
    let backoff_chrono = chrono::Duration::from_std(backoff).ok()?;
    let due = last.with_timezone(&Utc) + backoff_chrono;
    if due <= now {
        None
    } else {
        Some((due - now).to_std().unwrap_or(backoff))
    }
}

/// The consecutive-failure streak attributable to Job-DEADLINE kills, derived
/// from status alone: `consecutive_failures` iff the current `Bootstrapped` or
/// `BackendReachable` `False` condition carries a deadline reason
/// (`BootstrapDeadlineExceeded` / `ProbeDeadlineExceeded`), else `0` — a
/// crash-loop or outage streak must never inflate the deadline.
///
/// Threshold nuance (documented, not a bug): on a bootstrapped repository with
/// the default `failureThreshold: 3`, below-threshold probe failures leave
/// conditions untouched (the debounce), so escalation only engages once the
/// threshold crosses and the deadline reason lands on a condition. The
/// never-bootstrapped recycle route writes its `Bootstrapped=False` reason on
/// failure 1, so there escalation starts from attempt 2.
pub fn timeout_streak(conditions: &[Condition], consecutive_failures: i64) -> i64 {
    let deadline_reasoned = conditions.iter().any(|c| {
        (c.type_ == REPOSITORY_BOOTSTRAPPED_CONDITION || c.type_ == BACKEND_REACHABLE_CONDITION)
            && c.status == "False"
            && (c.reason == BOOTSTRAP_DEADLINE_EXCEEDED_REASON
                || c.reason == PROBE_DEADLINE_EXCEEDED_REASON)
    });
    if deadline_reasoned {
        consecutive_failures.max(0)
    } else {
        0
    }
}

/// The effective bootstrap-Job `activeDeadlineSeconds` under DEADLINE
/// ESCALATION (#414): `base << min(timeout_streak, 4)`, capped at
/// `max(base, STRICT_RETRY_BACKOFF_CAP_SECS)` — the operator applies the
/// "raise activeDeadlineSeconds until one connect succeeds" remediation
/// itself, in lockstep with the relaunch backoff (deadline ladder
/// 120→240→480→960→1800 at the default base), so a slow-but-alive backend
/// self-heals: one longer-deadline connect succeeds, maintenance (exempt from
/// the breaker for deadline degradation) compacts the index, the streak
/// resets, and the deadline returns to base. If even the 1800s rung fails, the
/// backend is genuinely dark and the honest breaker state stands.
///
/// Never reduces: the cap includes `base`, so an explicit user override larger
/// than the escalation cap is respected verbatim. `streak <= 0` → `base`.
///
/// Callers MUST feed the same status-derived value into
/// [`probe_attempt_timeout`] as into the Job's limits — both are computed
/// before the Job is fetched, and the streak only grows between a launch and
/// its poll, so the computed attempt window only ever WIDENS relative to the
/// running Job's real deadline. An attempt window narrower than the Job's
/// deadline would recycle a live Job mid-connect (the #273 class).
pub fn escalated_bootstrap_deadline(base_secs: i64, timeout_streak: i64) -> i64 {
    let exponent = timeout_streak.clamp(0, 4) as u32;
    let cap = (STRICT_RETRY_BACKOFF_CAP_SECS as i64).max(base_secs);
    base_secs.saturating_mul(1_i64 << exponent).min(cap)
}

/// Whether the repository's backend is CONFIRMED down or wiped: a
/// `BackendReachable=False` condition whose reason is `BackendUnreachable`
/// (the probe's connect verdict says the backend is not answering) or
/// `RepositoryVanished` (the backend answers but the kopia repository is
/// gone). A deny-list on purpose: a deadline-killed probe
/// (`ProbeDeadlineExceeded`, #414) says "connect slower than the Job
/// deadline", which is NOT confirmation of an outage — and any future reason
/// passes through as not-confirmed by construction, so the maintenance gate
/// can never silently widen its own deny-list.
pub fn backend_confirmed_down(conditions: &[Condition]) -> bool {
    conditions
        .iter()
        .find(|c| c.type_ == BACKEND_REACHABLE_CONDITION)
        .is_some_and(|c| {
            c.status == "False"
                && (c.reason == BACKEND_UNREACHABLE_REASON
                    || c.reason == REPOSITORY_VANISHED_REASON)
        })
}

/// Whether a `Maintenance` may spawn its mover Job against this repository —
/// the G7 gate's predicate (issue #413).
///
/// Keyed on **bootstrapped-before** (`status.uniqueId` set), NOT on
/// `phase == Ready`: an already-bootstrapped repository that went `Degraded`
/// *because* its index blobs need compacting (the probe deadline-kill spiral)
/// must still receive maintenance — maintenance IS the cure, and the
/// maintenance Job's own deadline is the 48h mover default, not the probe's
/// 120s. Deferring is reserved for the states where a maintenance pod is
/// doomed or dangerous:
///
/// - never bootstrapped (`Pending`/`Initializing`, or no `uniqueId`) — there
///   is nothing to maintain yet, spawning just produces a doomed pod;
/// - `Degraded` with the backend CONFIRMED unreachable or the repository
///   vanished ([`backend_confirmed_down`]) — the #345 breaker semantics;
/// - terminal `Failed` — the verdict (bad credentials, locked, …) dooms a
///   maintenance connect identically;
/// - `Unknown` — a newer controller's phase; consumers must hold, not proceed.
///
/// A `Degraded` repo whose mover merely crash-loops (`BootstrapJobFailed`,
/// OOM, bad image — no `BackendReachable=False` written) still gets
/// maintenance: the Jobs are bounded (deterministic per-slot names +
/// single-flight) and may well succeed. Exhaustive over [`RepositoryPhase`].
pub fn maintenance_may_proceed(
    phase: Option<&RepositoryPhase>,
    unique_id: Option<&str>,
    conditions: &[Condition],
) -> bool {
    match phase {
        Some(RepositoryPhase::Ready) => true,
        Some(RepositoryPhase::Degraded) => {
            unique_id.is_some() && !backend_confirmed_down(conditions)
        }
        Some(RepositoryPhase::Pending)
        | Some(RepositoryPhase::Initializing)
        | Some(RepositoryPhase::Failed)
        | Some(RepositoryPhase::Unknown(_))
        | None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(status: &str) -> Condition {
        Condition {
            type_: INDEX_BLOB_HEALTH_CONDITION.to_string(),
            status: status.to_string(),
            reason: "x".to_string(),
            message: "x".to_string(),
            last_transition_time: k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::now(),
            ),
            observed_generation: None,
        }
    }

    #[test]
    fn classify_respects_threshold_and_disable_sentinel() {
        assert_eq!(
            classify_index_blob_health(0, 1000),
            IndexBlobHealth::Healthy
        );
        assert_eq!(
            classify_index_blob_health(1000, 1000),
            IndexBlobHealth::Healthy
        );
        assert_eq!(
            classify_index_blob_health(1001, 1000),
            IndexBlobHealth::TooMany {
                count: 1001,
                threshold: 1000
            }
        );
        // 0 (and negatives, which the webhook rejects but we guard anyway) disable.
        assert_eq!(
            classify_index_blob_health(50_000, 0),
            IndexBlobHealth::Healthy
        );
        assert_eq!(
            classify_index_blob_health(50_000, -1),
            IndexBlobHealth::Healthy
        );
    }

    #[test]
    fn unhealthy_sets_false_condition_and_fires_event_once() {
        // First crossing: condition flips False AND an event is produced.
        let first = reconcile_index_blob_health(&[], 1448, 1000, Some(3));
        let c = first
            .conditions
            .iter()
            .find(|c| c.type_ == INDEX_BLOB_HEALTH_CONDITION)
            .unwrap();
        assert_eq!(c.status, "False");
        assert_eq!(c.reason, TOO_MANY_INDEX_BLOBS_REASON);
        let ev = first.event.expect("event on first crossing");
        assert_eq!(ev.reason, TOO_MANY_INDEX_BLOBS_REASON);
        assert!(ev.message.contains("1448"));
        assert!(ev.message.contains("takeoverPolicy: Force"));
        // #258: the reporter arrived here having already ruled out both of the original
        // remedies — maintenance was running, and raising the threshold only hides the
        // number. The epoch-advance gate was the actual cause, so the message must name it;
        // a warning whose every suggestion is a dead end is worse than no warning.
        assert!(
            ev.message.contains("spec.parameters.epoch.minDuration"),
            "the epoch remedy must be offered: {}",
            ev.message
        );

        // Still unhealthy on the next reconcile: condition stays False, NO new event.
        let again = reconcile_index_blob_health(&[cond("False")], 1500, 1000, Some(3));
        assert!(
            again.event.is_none(),
            "event must fire on transition only, not every reconcile"
        );
    }

    #[test]
    fn healthy_sets_true_condition_and_no_event() {
        let upd = reconcile_index_blob_health(&[cond("False")], 10, 1000, Some(3));
        let c = upd
            .conditions
            .iter()
            .find(|c| c.type_ == INDEX_BLOB_HEALTH_CONDITION)
            .unwrap();
        assert_eq!(c.status, "True");
        assert_eq!(c.reason, INDEX_BLOBS_HEALTHY_REASON);
        assert!(upd.event.is_none());
    }

    #[test]
    fn disabled_threshold_never_warns() {
        let upd = reconcile_index_blob_health(&[], 99_999, 0, Some(1));
        let c = upd
            .conditions
            .iter()
            .find(|c| c.type_ == INDEX_BLOB_HEALTH_CONDITION)
            .unwrap();
        assert_eq!(c.status, "True", "0 disables the warning");
        assert!(upd.event.is_none());
    }

    // ---- Part A: auto_create_allowed ---------------------------------------

    #[test]
    fn auto_create_only_on_first_bootstrap() {
        // Opted in, never bootstrapped → may create (first bootstrap).
        assert!(auto_create_allowed(true, None));
        // Once-Ready (pinned uniqueId) → NEVER create, even though create.enabled.
        // This is the load-bearing data-safety invariant.
        assert!(!auto_create_allowed(true, Some("kopia-unique-id")));
        // Not opted in → never create regardless.
        assert!(!auto_create_allowed(false, None));
        assert!(!auto_create_allowed(false, Some("kopia-unique-id")));
    }

    // ---- Part B: health_probe_due ------------------------------------------

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn probe_due_truth_table() {
        let interval = std::time::Duration::from_secs(1800);
        // Disabled → never due.
        assert!(!health_probe_due(
            true,
            false,
            Some(&t(0).to_rfc3339()),
            interval,
            t(10_000)
        ));
        // Never bootstrapped → never due (a probe of a non-existent repo is meaningless).
        assert!(!health_probe_due(false, true, None, interval, t(10_000)));
        // Enabled + bootstrapped + never probed → due.
        assert!(health_probe_due(true, true, None, interval, t(10_000)));
        // Enabled + bootstrapped + interval not elapsed → not due.
        assert!(!health_probe_due(
            true,
            true,
            Some(&t(9_000).to_rfc3339()),
            interval,
            t(10_000)
        ));
        // Enabled + bootstrapped + interval elapsed → due.
        assert!(health_probe_due(
            true,
            true,
            Some(&t(0).to_rfc3339()),
            interval,
            t(10_000)
        ));
        // Crucially NOT keyed on phase==Ready: a repo that already raised an alert
        // (and stayed Ready) keeps probing — `bootstrapped_before` stays true.
    }

    // ---- #273: the launch-side attempt stamp -------------------------------

    /// The attempt timeout used throughout these tests: the default Job deadline
    /// (120s) plus the grace window (60s) = 180s.
    fn attempt_timeout() -> std::time::Duration {
        probe_attempt_timeout(bootstrap_deadline_seconds(None))
    }

    #[test]
    fn probe_action_truth_table() {
        let interval = std::time::Duration::from_secs(1800);
        let to = attempt_timeout();
        let action = |ready, boot, enabled, last: Option<&str>, attempt: Option<&str>, now| {
            probe_action(ready, boot, enabled, last, attempt, interval, to, now)
        };

        // Ready + enabled + bootstrapped + never probed + no attempt → launch.
        assert_eq!(
            action(true, true, true, None, None, t(10_000)),
            ProbeAction::Launch
        );
        // THE #273 ASSERTION: with a fresh attempt stamped, the pass must NOT launch
        // (i.e. must not recycle) — it must wait for that Job's result. On the buggy
        // code this state did not exist and the Job was recycled forever.
        assert_eq!(
            action(
                true,
                true,
                true,
                None,
                Some(&t(9_950).to_rfc3339()),
                t(10_000)
            ),
            ProbeAction::AwaitingResult
        );
        // The liveness escape: once the attempt is older than the timeout (a crashed
        // controller, a TTL-reaped Job, a result ConfigMap that never materialised),
        // the probe must become launchable again rather than wedging forever.
        assert_eq!(
            action(
                true,
                true,
                true,
                None,
                Some(&t(9_000).to_rfc3339()),
                t(10_000)
            ),
            ProbeAction::Launch
        );
        // Fail-open: an unparseable stamp must never wedge the probe.
        assert_eq!(
            action(true, true, true, None, Some("not-a-timestamp"), t(10_000)),
            ProbeAction::Launch
        );
        // Timer not elapsed → idle.
        assert_eq!(
            action(
                true,
                true,
                true,
                Some(&t(9_000).to_rfc3339()),
                None,
                t(10_000)
            ),
            ProbeAction::Idle
        );
        // Timer elapsed → launch again (the probe re-fires on cadence).
        assert_eq!(
            action(true, true, true, Some(&t(0).to_rfc3339()), None, t(10_000)),
            ProbeAction::Launch
        );
        // Disabled / never bootstrapped → idle regardless.
        assert_eq!(
            action(true, true, false, None, None, t(10_000)),
            ProbeAction::Idle
        );
        assert_eq!(
            action(true, false, true, None, None, t(10_000)),
            ProbeAction::Idle
        );
    }

    /// A repository that is NOT `Ready` must never produce probe activity, even when
    /// every other input says a probe is due.
    ///
    /// Two distinct hazards. (1) A probe must not race a real (re-)bootstrap: without
    /// this guard a spec change opens a second livelock — the generation arm recycles
    /// the fresh strict result before it can write `observedGeneration`. (2) A
    /// terminally-`Failed` repository's lingering Job must not be re-read as a probe:
    /// `finalize_probe_failure` writes `phase: Ready` under `onFailure: Alert` (or
    /// below the failure threshold), so that would silently resurrect a failed
    /// repository and open every downstream `repository_ready` gate.
    #[test]
    fn a_repository_that_is_not_ready_never_probes() {
        let interval = std::time::Duration::from_secs(1800);
        let to = attempt_timeout();
        // Due by every other measure, but not Ready.
        assert_eq!(
            probe_action(false, true, true, None, None, interval, to, t(10_000)),
            ProbeAction::Idle
        );
        // Even with a fresh attempt stamped, a non-Ready repo never claims the result.
        let upd = probe_action(
            false,
            true,
            true,
            None,
            Some(&t(9_950).to_rfc3339()),
            interval,
            to,
            t(10_000),
        );
        assert_eq!(upd, ProbeAction::Idle);
        assert!(
            !upd.awaits_result(),
            "a Failed/Initializing repository must never treat a finished Job as its \
             probe result — finalize_probe_failure would flip it back to Ready"
        );
    }

    /// #273 encoded as an executable regression: drive the pure predicates through the
    /// real reconcile sequence and assert it CONVERGES.
    ///
    /// On the buggy code the cycle was: due → recycle Job → recreate → completes →
    /// still due (nothing ever stamped `lastProbeAt`, because the recycle returned
    /// before `finalize_*`) → recycle → … forever. The launch-side stamp is what makes
    /// the middle state (`AwaitingResult`) reachable, and therefore the timer clearable.
    #[test]
    fn probe_attempt_stamp_breaks_the_recycle_livelock() {
        let interval = std::time::Duration::from_secs(1800);
        let to = attempt_timeout();

        // Pass 1 — a Ready, bootstrapped repo with the probe on and nothing recorded.
        let mut last_probe_at: Option<String> = None;
        let mut probe_attempt_at: Option<String> = None;
        let first = probe_action(
            true,
            true,
            true,
            last_probe_at.as_deref(),
            probe_attempt_at.as_deref(),
            interval,
            to,
            t(10_000),
        );
        assert_eq!(first, ProbeAction::Launch);

        // The reconciler launches the Job and stamps the attempt (create-site write).
        probe_attempt_at = Some(t(10_000).to_rfc3339());

        // Pass 2 — the Job is running/finished. This MUST NOT be another Launch, or the
        // finished Job is recycled and we are back in the livelock.
        let second = probe_action(
            true,
            true,
            true,
            last_probe_at.as_deref(),
            probe_attempt_at.as_deref(),
            interval,
            to,
            t(10_020),
        );
        assert_eq!(
            second,
            ProbeAction::AwaitingResult,
            "#273: a launched probe must be awaited, not relaunched — relaunching \
             destroys the result that would stamp lastProbeAt"
        );
        assert!(
            second.awaits_result() && !second.launches(),
            "the finished Job must reach finalize_*, which is the only writer of \
             lastProbeAt"
        );

        // `finalize_*` runs: stamps lastProbeAt and CLEARS the attempt.
        last_probe_at = Some(t(10_020).to_rfc3339());
        probe_attempt_at = None;

        // Pass 3 — converged. Quiet until the interval elapses.
        assert_eq!(
            probe_action(
                true,
                true,
                true,
                last_probe_at.as_deref(),
                probe_attempt_at.as_deref(),
                interval,
                to,
                t(10_030),
            ),
            ProbeAction::Idle,
            "#273: the steady state after a completed probe must be quiet — a Launch \
             here means the bootstrap Job churns forever"
        );
        // ... and re-fires exactly once the interval has passed.
        assert_eq!(
            probe_action(
                true,
                true,
                true,
                last_probe_at.as_deref(),
                probe_attempt_at.as_deref(),
                interval,
                to,
                t(10_020 + 1800),
            ),
            ProbeAction::Launch,
            "the probe must still re-fire on cadence — fixing the loop must not mean \
             never probing again"
        );
    }

    /// A finished Job that no probe launched must never be claimed as a probe result.
    /// This is what used to fabricate `lastHealthyAt` from a connect nobody requested.
    #[test]
    fn a_job_no_probe_launched_is_never_read_as_a_probe() {
        let interval = std::time::Duration::from_secs(1800);
        let to = attempt_timeout();
        // No attempt stamped (the Job came from a spec-change re-bootstrap or a
        // reverify) → the pass never claims its result.
        let action = probe_action(
            true,
            true,
            true,
            Some(&t(9_990).to_rfc3339()),
            None,
            interval,
            to,
            t(10_000),
        );
        assert!(!action.awaits_result());
    }

    #[test]
    fn probe_attempt_timeout_outlives_the_job_deadline() {
        // The window must exceed the Job's own deadline, or an attempt is declared
        // abandoned while its Job is still legitimately running.
        let default = probe_attempt_timeout(bootstrap_deadline_seconds(None));
        assert!(
            default
                > std::time::Duration::from_secs(crate::consts::BOOTSTRAP_JOB_DEADLINE_SECS as u64),
            "the attempt window must outlive the Job deadline it waits on"
        );
        // A user raising activeDeadlineSeconds for a slow backend widens it too.
        assert!(probe_attempt_timeout(bootstrap_deadline_seconds(Some(900))) > default);
        assert_eq!(
            bootstrap_deadline_seconds(None),
            crate::consts::BOOTSTRAP_JOB_DEADLINE_SECS
        );
        assert_eq!(bootstrap_deadline_seconds(Some(42)), 42);
    }

    #[test]
    fn probe_attempt_pending_truth_table() {
        let to = std::time::Duration::from_secs(180);
        assert!(!probe_attempt_pending(None, to, t(10_000)), "absent");
        assert!(
            !probe_attempt_pending(Some("garbage"), to, t(10_000)),
            "unparseable must fail OPEN so a bad value cannot wedge the probe"
        );
        assert!(probe_attempt_pending(
            Some(&t(9_950).to_rfc3339()),
            to,
            t(10_000)
        ));
        assert!(!probe_attempt_pending(
            Some(&t(9_000).to_rfc3339()),
            to,
            t(10_000)
        ));
    }

    #[test]
    fn probe_attempt_health_patch_stamps_or_clears() {
        let now = t(200).to_rfc3339();
        assert_eq!(
            probe_attempt_health_patch(Some(&now))["probeAttemptAt"].as_str(),
            Some(now.as_str())
        );
        // A strict relaunch must CLEAR, and an empty object would not — the merge
        // patch needs an explicit null or the stale stamp survives.
        let cleared = probe_attempt_health_patch(None);
        assert!(
            cleared["probeAttemptAt"].is_null(),
            "must be an explicit null, got {cleared:?}"
        );
    }

    #[test]
    fn probe_failure_health_patch_clears_the_attempt_stamp() {
        // The trap: `RepositoryHealthStatus` fields are `skip_serializing_if =
        // "Option::is_none"`, so serializing the typed struct directly ELIDES the
        // `None` and the launch stamp survives in the stored object — leaving the repo
        // stuck `AwaitingResult` and making the next finished Job read as a probe.
        let health = RepositoryHealthStatus {
            last_probe_at: Some(t(100).to_rfc3339()),
            last_healthy_at: Some(t(50).to_rfc3339()),
            consecutive_probe_failures: Some(2),
            first_failure_at: Some(t(80).to_rfc3339()),
            probe_attempt_at: Some(t(90).to_rfc3339()),
        };
        let raw = serde_json::to_value(&health).unwrap();
        assert_eq!(
            raw["probeAttemptAt"].as_str(),
            Some(t(90).to_rfc3339().as_str()),
            "sanity: the typed struct round-trips the stamp"
        );

        let patch = probe_failure_health_patch(&health);
        assert!(
            patch["probeAttemptAt"].is_null(),
            "the failure patch must emit an explicit null to retire the attempt"
        );
        // ... while preserving everything the debounce depends on.
        assert_eq!(
            patch["lastProbeAt"].as_str(),
            Some(t(100).to_rfc3339().as_str())
        );
        assert_eq!(patch["consecutiveProbeFailures"].as_i64(), Some(2));
        assert_eq!(
            patch["firstFailureAt"].as_str(),
            Some(t(80).to_rfc3339().as_str())
        );
    }

    // ---- Part B: probe failure debounce + classification -------------------

    fn reachable_cond(status: &str, reason: &str) -> Condition {
        Condition {
            type_: BACKEND_REACHABLE_CONDITION.to_string(),
            status: status.to_string(),
            reason: reason.to_string(),
            message: "x".to_string(),
            last_transition_time: k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::now(),
            ),
            observed_generation: None,
        }
    }

    fn health_with_failures(n: i64) -> RepositoryHealthStatus {
        RepositoryHealthStatus {
            last_probe_at: Some(t(0).to_rfc3339()),
            last_healthy_at: Some(t(0).to_rfc3339()),
            consecutive_probe_failures: Some(n),
            first_failure_at: Some(t(0).to_rfc3339()),
            probe_attempt_at: None,
        }
    }

    #[test]
    fn probe_failure_debounces_until_threshold_then_fires_once() {
        let now = t(100).to_rfc3339();
        // 1st failure (threshold 3): counter increments, NO loud condition, NO event.
        let u1 = reconcile_probe_failure(&[], None, ProbeFailureKind::Vanished, 3, &now, Some(1));
        assert_eq!(u1.health.consecutive_probe_failures, Some(1));
        assert!(u1.event.is_none(), "no alert before the threshold");
        assert!(
            u1.conditions
                .iter()
                .all(|c| c.type_ != BACKEND_REACHABLE_CONDITION),
            "no BackendReachable condition during debounce"
        );

        // 2nd failure: still below threshold.
        let u2 = reconcile_probe_failure(
            &[],
            Some(&health_with_failures(1)),
            ProbeFailureKind::Vanished,
            3,
            &now,
            Some(1),
        );
        assert_eq!(u2.health.consecutive_probe_failures, Some(2));
        assert!(u2.event.is_none());

        // 3rd failure: crosses threshold → loud condition + event fire ONCE.
        let u3 = reconcile_probe_failure(
            &[],
            Some(&health_with_failures(2)),
            ProbeFailureKind::Vanished,
            3,
            &now,
            Some(1),
        );
        assert_eq!(u3.health.consecutive_probe_failures, Some(3));
        let c = u3
            .conditions
            .iter()
            .find(|c| c.type_ == BACKEND_REACHABLE_CONDITION)
            .expect("BackendReachable condition raised at threshold");
        assert_eq!(c.status, "False");
        assert_eq!(c.reason, REPOSITORY_VANISHED_REASON);
        let ev = u3.event.expect("event on threshold crossing");
        assert_eq!(ev.reason, REPOSITORY_VANISHED_REASON);
        assert!(ev.message.contains("VANISHED"));
        assert!(
            ev.message
                .to_lowercase()
                .contains("data blobs may still remain")
        );

        // 4th failure: still over threshold, same reason → NO repeat event.
        let u4 = reconcile_probe_failure(
            &[reachable_cond("False", REPOSITORY_VANISHED_REASON)],
            Some(&health_with_failures(3)),
            ProbeFailureKind::Vanished,
            3,
            &now,
            Some(1),
        );
        assert_eq!(u4.health.consecutive_probe_failures, Some(4));
        assert!(u4.event.is_none(), "no repeat event while steadily failing");
    }

    #[test]
    fn vanished_and_unreachable_are_distinct() {
        let now = t(100).to_rfc3339();
        let vanished = reconcile_probe_failure(
            &[],
            Some(&health_with_failures(0)),
            ProbeFailureKind::Vanished,
            1,
            &now,
            None,
        );
        assert_eq!(
            vanished
                .conditions
                .iter()
                .find(|c| c.type_ == BACKEND_REACHABLE_CONDITION)
                .unwrap()
                .reason,
            REPOSITORY_VANISHED_REASON
        );
        let unreachable = reconcile_probe_failure(
            &[],
            Some(&health_with_failures(0)),
            ProbeFailureKind::Unreachable,
            1,
            &now,
            None,
        );
        let c = unreachable
            .conditions
            .iter()
            .find(|c| c.type_ == BACKEND_REACHABLE_CONDITION)
            .unwrap();
        assert_eq!(c.reason, BACKEND_UNREACHABLE_REASON);
        // The unreachable message must NOT claim a vanish (no destructive nudge).
        let ev = unreachable.event.unwrap();
        assert!(!ev.message.contains("VANISHED"));
        assert!(ev.message.contains("NOT treated as a wipe"));
        // #414: the third kind — a deadline-killed probe — carries its own
        // reason; it must claim neither a vanish nor an outage.
        let timed_out = reconcile_probe_failure(
            &[],
            Some(&health_with_failures(0)),
            ProbeFailureKind::TimedOut,
            1,
            &now,
            None,
        );
        let c = timed_out
            .conditions
            .iter()
            .find(|c| c.type_ == BACKEND_REACHABLE_CONDITION)
            .unwrap();
        assert_eq!(c.reason, PROBE_DEADLINE_EXCEEDED_REASON);
        assert_ne!(c.reason, BACKEND_UNREACHABLE_REASON);
        let ev = timed_out.event.unwrap();
        assert_eq!(ev.action, RAISE_BOOTSTRAP_DEADLINE_ACTION);
        assert!(!ev.message.contains("VANISHED"), "{}", ev.message);
        assert!(
            !ev.message.contains("credentials/lock failed"),
            "{}",
            ev.message
        );
    }

    /// #414 message obligations for the deadline-kill probe alert: the what
    /// (deadline kill + streak), the why (cold-cache connect scales with the
    /// index), the not-an-outage disclaimer, and the fix (raise the deadline;
    /// maintenance keeps running and shrinks connect time).
    #[test]
    fn deadline_kill_message_obligations() {
        let now = t(100).to_rfc3339();
        let upd = reconcile_probe_failure(
            &[],
            Some(&health_with_failures(2)),
            ProbeFailureKind::TimedOut,
            3,
            &now,
            None,
        );
        let msg = &upd.event.expect("threshold crossed → event").message;
        assert!(msg.contains("activeDeadlineSeconds"), "{msg}");
        assert!(
            msg.contains("3 consecutive deadline-killed probes"),
            "{msg}"
        );
        assert!(msg.contains("may be reachable but slow"), "{msg}");
        assert!(msg.contains("NOT evidence of an outage"), "{msg}");
        assert!(
            msg.contains("spec.bootstrap.failurePolicy.activeDeadlineSeconds"),
            "{msg}"
        );
        assert!(msg.contains("maintenance"), "{msg}");
        assert!(msg.contains("keeps running"), "{msg}");
    }

    #[test]
    fn reason_change_over_threshold_re_fires_event() {
        let now = t(100).to_rfc3339();
        // Already over threshold as Unreachable; a probe now classifies Vanished
        // (an escalation) → the event must fire again so the alert isn't swallowed.
        let upd = reconcile_probe_failure(
            &[reachable_cond("False", BACKEND_UNREACHABLE_REASON)],
            Some(&health_with_failures(5)),
            ProbeFailureKind::Vanished,
            3,
            &now,
            None,
        );
        let ev = upd.event.expect("reason change re-fires the event");
        assert_eq!(ev.reason, REPOSITORY_VANISHED_REASON);
    }

    #[test]
    fn probe_success_resets_debounce_and_clears_condition() {
        let now = t(200).to_rfc3339();
        let upd = reconcile_probe_success(
            &[reachable_cond("False", REPOSITORY_VANISHED_REASON)],
            &now,
            Some(2),
        );
        let c = upd
            .conditions
            .iter()
            .find(|c| c.type_ == BACKEND_REACHABLE_CONDITION)
            .unwrap();
        assert_eq!(c.status, "True");
        assert_eq!(c.reason, BACKEND_REACHABLE_REASON);
        assert_eq!(upd.health.consecutive_probe_failures, None, "streak reset");
        assert_eq!(upd.health.first_failure_at, None);
        assert_eq!(upd.health.last_healthy_at.as_deref(), Some(now.as_str()));
        assert!(upd.event.is_none());
    }

    // ---- #345 M3: the unified success fold --------------------------------

    /// A fully-seeded, healthy `status.health` — the steady state.
    fn seeded_healthy() -> RepositoryHealthStatus {
        RepositoryHealthStatus {
            last_probe_at: Some(t(100).to_rfc3339()),
            last_healthy_at: Some(t(100).to_rfc3339()),
            consecutive_probe_failures: None,
            first_failure_at: None,
            probe_attempt_at: None,
        }
    }

    #[test]
    fn success_fold_truth_table() {
        let now = t(200).to_rfc3339();

        // Probe run → always folds (the probe deletes its Job, so once-only).
        let probe = success_fold(true, Some(&seeded_healthy()), true, &now).expect("probe folds");
        assert_eq!(probe.reason, SuccessFoldReason::Probe);

        // Strict + lastProbeAt None (fresh repo or pre-#345 upgrade) → seed, so
        // health_probe_due does not fire a redundant probe Job fleet-wide.
        let seed = success_fold(false, None, false, &now).expect("seeding folds");
        assert_eq!(seed.reason, SuccessFoldReason::Seed);
        let seed2 = success_fold(false, Some(&RepositoryHealthStatus::default()), false, &now)
            .expect("empty health seeds too");
        assert_eq!(seed2.reason, SuccessFoldReason::Seed);

        // Strict + a recorded failing streak → heal (clears the streak; this is
        // what closes the breaker on a successful re-connect in M4).
        let heal = success_fold(false, Some(&health_with_failures(2)), false, &now)
            .expect("a failing streak heals");
        assert_eq!(heal.reason, SuccessFoldReason::Heal);

        // Strict + firstFailureAt lingering (streak count already cleared) → heal.
        let lingering = RepositoryHealthStatus {
            first_failure_at: Some(t(80).to_rfc3339()),
            ..seeded_healthy()
        };
        assert_eq!(
            success_fold(false, Some(&lingering), true, &now)
                .expect("lingering firstFailureAt heals")
                .reason,
            SuccessFoldReason::Heal
        );

        // Strict + seeded + healthy counters but BackendReachable not True
        // (stale False, or the condition is absent) → heal the condition.
        assert_eq!(
            success_fold(false, Some(&seeded_healthy()), false, &now)
                .expect("a non-True BackendReachable heals")
                .reason,
            SuccessFoldReason::Heal
        );

        // THE no-op case: strict + seeded + healthy + condition True → None.
        // This is what keeps the lingering-Job re-read byte-stable (no hot loop).
        assert!(
            success_fold(false, Some(&seeded_healthy()), true, &now).is_none(),
            "the steady-state pass must contribute NOTHING to the status patch"
        );
    }

    #[test]
    fn success_fold_converges_after_one_write() {
        // Apply a Seed write's effect (lastProbeAt stamped, condition True) and
        // assert the next lingering-Job re-read folds nothing — the whole point
        // of the conditional fold is that pass 2..N are byte-stable no-ops.
        let now = t(200).to_rfc3339();
        let fold = success_fold(false, None, false, &now).expect("pass 1 seeds");
        assert_eq!(fold.reason, SuccessFoldReason::Seed);
        let after = RepositoryHealthStatus {
            last_probe_at: Some(now.clone()),
            last_healthy_at: Some(now.clone()),
            consecutive_probe_failures: None,
            first_failure_at: None,
            probe_attempt_at: None,
        };
        assert!(
            success_fold(false, Some(&after), true, &t(210).to_rfc3339()).is_none(),
            "pass 2 must be a no-op"
        );
    }

    #[test]
    fn success_fold_patch_reuses_the_explicit_null_clears() {
        // The fold's patch must be probe_success_health_patch's shape: a heal
        // that omitted the RFC 7386 nulls would leave the failing streak in the
        // stored object and re-fire the loud alert on the next single failure.
        let now = t(200).to_rfc3339();
        let fold = success_fold(false, Some(&health_with_failures(3)), false, &now).unwrap();
        assert_eq!(fold.health_patch, probe_success_health_patch(&now));
        assert!(fold.health_patch["consecutiveProbeFailures"].is_null());
        assert!(fold.health_patch["firstFailureAt"].is_null());
        assert!(fold.health_patch["probeAttemptAt"].is_null());
    }

    #[test]
    fn backend_reachable_true_reads_only_the_true_state() {
        assert!(!backend_reachable_true(&[]), "absent is not True");
        assert!(!backend_reachable_true(&[reachable_cond(
            "False",
            REPOSITORY_VANISHED_REASON
        )]));
        assert!(backend_reachable_true(&[reachable_cond(
            "True",
            BACKEND_REACHABLE_REASON
        )]));
        // Other condition types are ignored.
        assert!(!backend_reachable_true(&[cond("True")]));
    }

    #[test]
    fn probe_success_health_patch_emits_explicit_nulls_to_clear_the_debounce() {
        // The patch MUST carry explicit JSON `null` for the two debounce fields:
        // status is written with a JSON merge-patch (RFC 7386), and an omitted key
        // leaves the prior failing streak in place — which would re-fire the loud
        // alert on the very next single failure, defeating `failureThreshold`.
        let now = t(200).to_rfc3339();
        let patch = probe_success_health_patch(&now);
        assert_eq!(patch["lastProbeAt"].as_str(), Some(now.as_str()));
        assert_eq!(patch["lastHealthyAt"].as_str(), Some(now.as_str()));
        assert!(
            patch["consecutiveProbeFailures"].is_null(),
            "must be explicit null so the merge clears it, got {:?}",
            patch["consecutiveProbeFailures"]
        );
        assert!(
            patch["firstFailureAt"].is_null(),
            "must be explicit null so the merge clears it, got {:?}",
            patch["firstFailureAt"]
        );
        // #273: the launch-side attempt stamp must be retired here too. If it survives,
        // the next pass still reads `AwaitingResult` and the probe never re-fires.
        assert!(
            patch["probeAttemptAt"].is_null(),
            "must be explicit null so the merge retires the attempt, got {:?}",
            patch["probeAttemptAt"]
        );
    }

    // ---- #345 M4: the circuit breaker --------------------------------------

    #[test]
    fn breaker_verdict_truth_table() {
        use ProbeOnFailure::{Alert, Degrade};
        // Below the threshold: never open, either mode.
        assert_eq!(breaker_verdict(0, 3, Degrade), BreakerVerdict::StayReady);
        assert_eq!(breaker_verdict(2, 3, Degrade), BreakerVerdict::StayReady);
        assert_eq!(breaker_verdict(2, 3, Alert), BreakerVerdict::StayReady);
        // At the threshold: Degrade opens, Alert never does.
        assert_eq!(breaker_verdict(3, 3, Degrade), BreakerVerdict::Open);
        assert_eq!(breaker_verdict(3, 3, Alert), BreakerVerdict::StayReady);
        // Above the threshold: same split — Alert keeps the pre-#345 contract
        // no matter how long the outage runs.
        assert_eq!(breaker_verdict(50, 3, Degrade), BreakerVerdict::Open);
        assert_eq!(breaker_verdict(50, 3, Alert), BreakerVerdict::StayReady);
        // A non-positive threshold clamps to 1 (mirroring reconcile_probe_failure),
        // so a single failure opens under Degrade.
        assert_eq!(breaker_verdict(1, 0, Degrade), BreakerVerdict::Open);
        assert_eq!(breaker_verdict(0, 0, Degrade), BreakerVerdict::StayReady);
    }

    #[test]
    fn both_probe_failure_kinds_open_the_breaker() {
        // `breaker_verdict` takes no kind on purpose: a backend that cannot be
        // confirmed healthy dooms backups whether it vanished, refuses
        // connections, or is merely too slow for the probe deadline. The
        // kind-mapped surfaces (reason/action/message) must still be distinct
        // and stable.
        assert_eq!(
            breaker_reason(ProbeFailureKind::Vanished),
            REPOSITORY_VANISHED_REASON
        );
        assert_eq!(
            breaker_reason(ProbeFailureKind::Unreachable),
            BACKEND_UNREACHABLE_REASON
        );
        assert_eq!(
            breaker_reason(ProbeFailureKind::TimedOut),
            PROBE_DEADLINE_EXCEEDED_REASON
        );
        assert_eq!(
            breaker_action(ProbeFailureKind::Vanished),
            VERIFY_BACKEND_ACTION
        );
        assert_eq!(
            breaker_action(ProbeFailureKind::Unreachable),
            CHECK_BACKEND_ACTION
        );
        assert_eq!(
            breaker_action(ProbeFailureKind::TimedOut),
            RAISE_BOOTSTRAP_DEADLINE_ACTION
        );
        for kind in [
            ProbeFailureKind::Vanished,
            ProbeFailureKind::Unreachable,
            ProbeFailureKind::TimedOut,
        ] {
            let msg = breaker_open_message(kind);
            assert!(msg.contains("paused until a connect succeeds"), "{msg}");
            assert!(msg.contains("retrying with backoff"), "{msg}");
        }
        // The two pre-#414 messages are pinned BYTE-IDENTICALLY: guarded no-op
        // status writes (and any operator tooling matching on them) depend on
        // the exact bytes not drifting when a new kind is added.
        assert_eq!(
            breaker_open_message(ProbeFailureKind::Vanished),
            "the backend is reachable but the kopia repository is absent — the circuit breaker \
             is open: backups, maintenance, and replication are paused until a connect succeeds; \
             retrying with backoff (recovery is automatic; kopiur never auto-recreates)"
        );
        assert_eq!(
            breaker_open_message(ProbeFailureKind::Unreachable),
            "the repository backend is unreachable — the circuit breaker is open: backups, \
             maintenance, and replication are paused until a connect succeeds; retrying with \
             backoff (recovery is automatic)"
        );
        // The vanished message must still refuse the destructive nudge.
        assert!(breaker_open_message(ProbeFailureKind::Vanished).contains("never auto-recreates"));
        // Only the TimedOut message says maintenance keeps running (#413): the
        // confirmed-down kinds really do pause maintenance, so their text must
        // NOT be softened.
        let timed_out = breaker_open_message(ProbeFailureKind::TimedOut);
        assert!(timed_out.contains("maintenance still runs"), "{timed_out}");
        assert!(
            !timed_out.contains("maintenance, and replication are paused"),
            "{timed_out}"
        );
        assert!(
            timed_out.contains("spec.bootstrap.failurePolicy.activeDeadlineSeconds"),
            "{timed_out}"
        );
        assert_eq!(ProbeFailureKind::Vanished.label(), "vanished");
        assert_eq!(ProbeFailureKind::Unreachable.label(), "unreachable");
        assert_eq!(ProbeFailureKind::TimedOut.label(), "timed_out");
    }

    #[test]
    fn probe_failure_phase_maps_the_verdict_to_phase_and_requeue() {
        let steady = std::time::Duration::from_secs(300);
        // StayReady: the pre-#345 alert-only contract — phase Ready written
        // explicitly (a stray Degraded self-heals), steady probe cadence.
        let p = probe_failure_phase(
            BreakerVerdict::StayReady,
            ProbeFailureKind::Unreachable,
            steady,
        );
        assert!(!p.opened);
        assert_eq!(p.phase, RepositoryPhase::Ready);
        assert_eq!(p.phase_str, "Ready");
        assert_eq!(p.ready_reason, "Bootstrapped");
        assert!(p.ready_message.contains("backups"));
        assert_eq!(p.requeue, steady);
        // Open: Degraded + the byte-stable breaker message + a short hop into
        // the strict retry loop (which strict_retry_holdoff then governs).
        for kind in [
            ProbeFailureKind::Vanished,
            ProbeFailureKind::Unreachable,
            ProbeFailureKind::TimedOut,
        ] {
            let p = probe_failure_phase(BreakerVerdict::Open, kind, steady);
            assert!(p.opened);
            assert_eq!(p.phase, RepositoryPhase::Degraded);
            assert_eq!(p.phase_str, "Degraded");
            assert_eq!(p.ready_reason, breaker_reason(kind));
            assert_eq!(p.ready_message, breaker_open_message(kind));
            assert!(p.requeue < steady);
        }
    }

    #[test]
    fn launch_phase_keeps_degraded_and_initializes_everything_else() {
        // Audit 3d: a Degraded repo's strict retry must not flap the phase to
        // Initializing every cycle (phase-keyed alerts/gauges would break).
        assert_eq!(launch_phase(Some(&RepositoryPhase::Degraded)), "Degraded");
        // Every other pre-Ready state keeps the pre-M4 Initializing.
        assert_eq!(launch_phase(None), "Initializing");
        assert_eq!(
            launch_phase(Some(&RepositoryPhase::Pending)),
            "Initializing"
        );
        assert_eq!(
            launch_phase(Some(&RepositoryPhase::Initializing)),
            "Initializing"
        );
        assert_eq!(launch_phase(Some(&RepositoryPhase::Ready)), "Initializing");
        assert_eq!(launch_phase(Some(&RepositoryPhase::Failed)), "Initializing");
        // Not a known-paused `Degraded` repository, so the launch reports plain
        // first-bootstrap progress rather than inventing a breaker state.
        assert_eq!(
            launch_phase(Some(&RepositoryPhase::Unknown("Upgrading".into()))),
            "Initializing"
        );
    }

    #[test]
    fn strict_retry_backoff_doubles_from_120s_and_caps_at_the_shared_cap() {
        let secs = |n: i64| strict_retry_backoff(n).as_secs();
        assert_eq!(secs(0), 120);
        assert_eq!(secs(1), 240);
        assert_eq!(secs(2), 480);
        assert_eq!(secs(3), 960);
        // #415: the cap is the shared const (1800s = ~2 attempts/hour against
        // a paid-per-request store), so the curve and any consumer deriving
        // budgets from it can never disagree.
        assert_eq!(secs(4), STRICT_RETRY_BACKOFF_CAP_SECS);
        assert_eq!(secs(5), STRICT_RETRY_BACKOFF_CAP_SECS);
        assert_eq!(
            secs(1_000_000),
            STRICT_RETRY_BACKOFF_CAP_SECS,
            "huge streaks must not overflow"
        );
        assert_eq!(secs(-1), 120, "negative input is the base");
        assert_eq!(secs(i64::MIN), 120);
    }

    #[test]
    fn strict_retry_holdoff_gates_the_relaunch_and_fails_open() {
        let stamp = t(10_000).to_rfc3339();
        // Degraded + streak 1 + fresh failure → hold for the remaining base 120s.
        let held = strict_retry_holdoff(true, 1, Some(&stamp), t(10_030))
            .expect("a fresh failure must hold the relaunch");
        assert_eq!(held.as_secs(), 90);
        // Streak 3 → backoff(2) = 480s from the last failure.
        let held = strict_retry_holdoff(true, 3, Some(&stamp), t(10_030)).unwrap();
        assert_eq!(held.as_secs(), 450);
        // Elapsed → launch now.
        assert!(strict_retry_holdoff(true, 1, Some(&stamp), t(10_200)).is_none());
        // Fail OPEN on every edge: not Degraded, no streak, absent/garbage stamp.
        assert!(strict_retry_holdoff(false, 5, Some(&stamp), t(10_030)).is_none());
        assert!(strict_retry_holdoff(true, 0, Some(&stamp), t(10_030)).is_none());
        assert!(strict_retry_holdoff(true, -2, Some(&stamp), t(10_030)).is_none());
        assert!(strict_retry_holdoff(true, 5, None, t(10_030)).is_none());
        assert!(strict_retry_holdoff(true, 5, Some("garbage"), t(10_030)).is_none());
    }

    /// The full breaker episode, driven through the pure fns in the order the
    /// reconcilers compose them (the `probe_attempt_stamp_breaks_the_recycle_livelock`
    /// style): Ready → probe failures to the threshold → the verdict opens →
    /// the phase patch is `Degraded` → strict retry failures keep the phase
    /// stable and grow the streak → a strict success heals everything.
    #[test]
    fn breaker_opens_on_threshold_holds_through_retries_and_heals_on_success() {
        let threshold = 3;
        let on_failure = ProbeOnFailure::Degrade;

        // Probe failures 1 and 2: streak grows, verdict stays Ready — the
        // finalize writes phase: Ready exactly as before (alert debounce).
        let now = t(100).to_rfc3339();
        let u1 = reconcile_probe_failure(
            &[],
            None,
            ProbeFailureKind::Unreachable,
            threshold,
            &now,
            Some(1),
        );
        assert_eq!(u1.health.consecutive_probe_failures, Some(1));
        assert_eq!(
            breaker_verdict(1, threshold, on_failure),
            BreakerVerdict::StayReady,
            "below the threshold the phase patch must stay Ready"
        );
        let u2 = reconcile_probe_failure(
            &[],
            Some(&u1.health),
            ProbeFailureKind::Unreachable,
            threshold,
            &now,
            Some(1),
        );
        assert_eq!(
            breaker_verdict(2, threshold, on_failure),
            BreakerVerdict::StayReady
        );

        // Probe failure 3: the threshold crossing. The loud condition + event
        // fire AND the verdict opens — the finalize patches phase: Degraded
        // with the stable breaker message on the Ready condition.
        let u3 = reconcile_probe_failure(
            &[],
            Some(&u2.health),
            ProbeFailureKind::Unreachable,
            threshold,
            &now,
            Some(1),
        );
        assert_eq!(u3.health.consecutive_probe_failures, Some(3));
        assert!(u3.event.is_some(), "the alert fires at the crossing");
        assert_eq!(
            breaker_verdict(3, threshold, on_failure),
            BreakerVerdict::Open
        );
        let open_conditions = crate::io::set_ready(
            &u3.conditions,
            Some(1),
            crate::io::ready_outcome_for_phase(&RepositoryPhase::Degraded),
            breaker_reason(ProbeFailureKind::Unreachable),
            breaker_open_message(ProbeFailureKind::Unreachable),
        );
        let ready = open_conditions
            .iter()
            .find(|c| c.type_ == crate::consts::READY_CONDITION)
            .unwrap();
        assert_eq!(ready.status, "False");
        assert_eq!(ready.reason, BACKEND_UNREACHABLE_REASON);
        assert!(ready.message.contains("paused until a connect succeeds"));
        let reconciling = open_conditions
            .iter()
            .find(|c| c.type_ == crate::consts::RECONCILING_CONDITION)
            .unwrap();
        assert_eq!(
            reconciling.status, "True",
            "Degraded is kstatus-Reconciling (self-healing), never Stalled"
        );
        // The failure patch clears the attempt stamp and pins the streak.
        let patch = probe_failure_health_patch(&u3.health);
        assert_eq!(patch["consecutiveProbeFailures"].as_i64(), Some(3));
        assert!(patch["probeAttemptAt"].is_null());

        // While open: the strict retry launch keeps the phase Degraded (no
        // Initializing flap) and is rate-limited by the holdoff...
        assert_eq!(launch_phase(Some(&RepositoryPhase::Degraded)), "Degraded");
        let last_failure = u3.health.last_probe_at.clone().unwrap();
        assert!(
            strict_retry_holdoff(true, 3, Some(&last_failure), t(110)).is_some(),
            "a fresh failure holds the relaunch for the backoff window"
        );
        // ... and a failed strict retry folds through the SAME sensor: the
        // streak keeps climbing (the outage-duration counter) with no repeat
        // event (same reason, already over threshold).
        let retry_now = t(700).to_rfc3339();
        let u4 = reconcile_probe_failure(
            &u3.conditions,
            Some(&u3.health),
            ProbeFailureKind::Unreachable,
            threshold,
            &retry_now,
            Some(1),
        );
        assert_eq!(u4.health.consecutive_probe_failures, Some(4));
        assert!(u4.event.is_none(), "no event spam while steadily failing");
        assert_eq!(
            breaker_verdict(4, threshold, on_failure),
            BreakerVerdict::Open,
            "the breaker stays open while the streak persists"
        );

        // Strict success: the unified success fold heals — streak cleared,
        // BackendReachable=True — which is what closes the breaker (the
        // success arm writes phase: Ready alongside this fold).
        let heal_now = t(1_300).to_rfc3339();
        let fold = success_fold(false, Some(&u4.health), false, &heal_now)
            .expect("a recorded streak must heal on any successful strict connect");
        assert_eq!(fold.reason, SuccessFoldReason::Heal);
        assert!(fold.health_patch["consecutiveProbeFailures"].is_null());
        assert!(fold.health_patch["firstFailureAt"].is_null());
        let healed = reconcile_probe_success(&u4.conditions, &heal_now, Some(1));
        let c = healed
            .conditions
            .iter()
            .find(|c| c.type_ == BACKEND_REACHABLE_CONDITION)
            .unwrap();
        assert_eq!(c.status, "True");
        // After healing, the sensor state folds nothing more (byte-stable
        // steady state) — the episode is over.
        let after = RepositoryHealthStatus {
            last_probe_at: Some(heal_now.clone()),
            last_healthy_at: Some(heal_now.clone()),
            consecutive_probe_failures: None,
            first_failure_at: None,
            probe_attempt_at: None,
        };
        assert!(success_fold(false, Some(&after), true, &t(1_400).to_rfc3339()).is_none());
    }

    /// `onFailure: Alert` preserves the OLD contract end-to-end: any streak,
    /// however long, yields `StayReady` — the finalize keeps writing
    /// `phase: Ready` and only the condition/event/metric fire.
    #[test]
    fn alert_mode_never_opens_the_breaker() {
        for streak in [1, 3, 10, 1_000] {
            assert_eq!(
                breaker_verdict(streak, 3, ProbeOnFailure::Alert),
                BreakerVerdict::StayReady,
                "streak {streak} must stay Ready under Alert"
            );
        }
    }

    /// #415 relaunch metronome, encoded as an executable regression. The
    /// result-less recycle route used to write NO `status.health`: the
    /// holdoff's `lastProbeAt` anchor went stale, `elapsed` exceeded any
    /// backoff window, and the gate failed open into a fresh cold-cache
    /// connect every ~2.5 minutes, forever (24 billed attempts/hour against
    /// B2). With the route folding `failure_streak_health` per failure, the
    /// holdoff re-arms each cycle and the cadence decays
    /// 120→240→480→960→1800s.
    #[test]
    fn recycled_bootstrap_failures_rearm_the_holdoff() {
        // The BUG, as the contrast case: streak recorded once (by the original
        // probe failures), never re-stamped by the recycle route. Once the
        // stale anchor's backoff window has elapsed, every subsequent pass
        // launches immediately — the metronome.
        let stale_anchor = t(0).to_rfc3339();
        for pass in 1..5 {
            let now = t(600 + pass * 150); // any time past the stale window
            assert!(
                strict_retry_holdoff(true, 3, Some(&stale_anchor), now).is_none(),
                "pass {pass}: a never-re-stamped anchor fails open — the pre-fix bug"
            );
        }

        // The FIX: each failed relaunch folds the streak++ and a fresh anchor,
        // so the gate holds for the full (growing) backoff after every failure.
        let mut health: Option<RepositoryHealthStatus> = None;
        let mut clock = 0i64;
        for (failure_n, expected_hold) in [(1, 120u64), (2, 240), (3, 480), (4, 960), (5, 1800)] {
            let now = t(clock).to_rfc3339();
            let folded = failure_streak_health(health.as_ref(), &now);
            assert_eq!(folded.consecutive_probe_failures, Some(failure_n));
            // Immediately after the failure the relaunch is gated for the full
            // backoff window…
            let held = strict_retry_holdoff(
                true,
                folded.consecutive_probe_failures.unwrap(),
                folded.last_probe_at.as_deref(),
                t(clock),
            )
            .unwrap_or_else(|| panic!("failure {failure_n} must re-arm the holdoff"));
            assert_eq!(
                held.as_secs(),
                expected_hold,
                "failure {failure_n} holds for the {expected_hold}s rung"
            );
            // …and releases once the window elapses (no wedge), which is when
            // the next attempt launches (and, here, fails again).
            assert!(
                strict_retry_holdoff(
                    true,
                    folded.consecutive_probe_failures.unwrap(),
                    folded.last_probe_at.as_deref(),
                    t(clock + expected_hold as i64),
                )
                .is_none(),
                "failure {failure_n}: the gate must release after {expected_hold}s"
            );
            clock += expected_hold as i64;
            health = Some(folded);
        }
    }

    /// The recycle route's fold IS the probe fold: `failure_streak_health`
    /// must stay byte-equal to what `reconcile_probe_failure` records, across
    /// prior-streak shapes, or the two writers drift apart.
    #[test]
    fn failure_streak_health_matches_the_probe_fold() {
        let now = t(500).to_rfc3339();
        for prior in [
            None,
            Some(health_with_failures(0)),
            Some(health_with_failures(3)),
            // A continuing streak with a missing firstFailureAt heals it.
            Some(RepositoryHealthStatus {
                last_probe_at: Some(t(100).to_rfc3339()),
                last_healthy_at: None,
                consecutive_probe_failures: Some(2),
                first_failure_at: None,
                probe_attempt_at: Some(t(90).to_rfc3339()),
            }),
        ] {
            let folded = failure_streak_health(prior.as_ref(), &now);
            let probe = reconcile_probe_failure(
                &[],
                prior.as_ref(),
                ProbeFailureKind::Unreachable,
                3,
                &now,
                None,
            );
            assert_eq!(folded, probe.health, "prior {prior:?}");
        }
    }

    /// The recycle route's health patch must clear `probeAttemptAt` with an
    /// explicit JSON null — every field is skip_serializing_if, so a direct
    /// serialize can never clear it through a merge patch (the #273 class).
    #[test]
    fn recycle_route_health_patch_emits_the_explicit_null() {
        let folded = failure_streak_health(Some(&health_with_failures(1)), &t(100).to_rfc3339());
        let patch = probe_failure_health_patch(&folded);
        assert!(
            patch["probeAttemptAt"].is_null(),
            "must be an explicit null, got {patch:?}"
        );
        assert_eq!(patch["consecutiveProbeFailures"].as_i64(), Some(2));
        assert!(
            patch["lastProbeAt"].is_string(),
            "the holdoff anchor is stamped"
        );
        assert!(patch["firstFailureAt"].is_string());
    }

    /// The reset half of #415: after recycle-route failures, one successful
    /// bootstrap folds Heal and nulls the streak/anchor, so the next episode
    /// starts back at the 120s rung.
    #[test]
    fn success_clears_a_recycle_streak() {
        let now = t(1_000).to_rfc3339();
        let after_failures = failure_streak_health(
            Some(&failure_streak_health(None, &t(500).to_rfc3339())),
            &t(700).to_rfc3339(),
        );
        assert_eq!(after_failures.consecutive_probe_failures, Some(2));
        let fold = success_fold(false, Some(&after_failures), true, &now)
            .expect("a recorded streak must fold a Heal");
        let patch = fold.health_patch;
        assert!(patch["consecutiveProbeFailures"].is_null(), "{patch:?}");
        assert!(patch["firstFailureAt"].is_null(), "{patch:?}");
        assert!(patch["probeAttemptAt"].is_null(), "{patch:?}");
    }

    /// #414 deadline escalation: base << min(streak, 4), capped at
    /// max(base, the shared backoff cap), never reducing an explicit override.
    #[test]
    fn escalated_deadline_ladder() {
        // Default base: the ladder runs in lockstep with the relaunch backoff.
        for (streak, expected) in [(0, 120), (1, 240), (2, 480), (3, 960), (4, 1800), (5, 1800)] {
            assert_eq!(
                escalated_bootstrap_deadline(120, streak),
                expected,
                "streak {streak}"
            );
        }
        // An explicit override below the cap escalates from ITS base…
        assert_eq!(escalated_bootstrap_deadline(900, 1), 1800);
        // …and one above the cap is respected verbatim, never reduced.
        assert_eq!(escalated_bootstrap_deadline(7_200, 0), 7_200);
        assert_eq!(escalated_bootstrap_deadline(7_200, 3), 7_200);
        // Degenerate inputs stay safe.
        assert_eq!(escalated_bootstrap_deadline(120, -1), 120);
        assert_eq!(escalated_bootstrap_deadline(120, i64::MAX), 1800);
        assert_eq!(escalated_bootstrap_deadline(i64::MAX, 4), i64::MAX);
        // The attempt window derived from the escalated value only ever widens
        // as the streak grows — the #273-class undercut is unrepresentable.
        for streak in 0..6 {
            assert!(
                probe_attempt_timeout(escalated_bootstrap_deadline(120, streak + 1))
                    >= probe_attempt_timeout(escalated_bootstrap_deadline(120, streak))
            );
        }
    }

    /// Escalation is keyed on DEADLINE-reasoned failures only: a crash-loop or
    /// outage streak must never inflate the deadline.
    #[test]
    fn timeout_streak_only_counts_deadline_reasons() {
        let boot_false = |reason: &str| Condition {
            type_: REPOSITORY_BOOTSTRAPPED_CONDITION.to_string(),
            status: "False".to_string(),
            reason: reason.to_string(),
            message: "m".to_string(),
            last_transition_time: k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::now(),
            ),
            observed_generation: None,
        };
        // Deadline reasons on either condition count the full streak.
        assert_eq!(
            timeout_streak(&[boot_false(BOOTSTRAP_DEADLINE_EXCEEDED_REASON)], 3),
            3
        );
        assert_eq!(
            timeout_streak(
                &[reachable_cond("False", PROBE_DEADLINE_EXCEEDED_REASON)],
                2
            ),
            2
        );
        // Non-deadline reasons — and True conditions — contribute nothing.
        assert_eq!(timeout_streak(&[boot_false("BootstrapJobFailed")], 3), 0);
        assert_eq!(
            timeout_streak(&[reachable_cond("False", BACKEND_UNREACHABLE_REASON)], 5),
            0
        );
        assert_eq!(
            timeout_streak(&[reachable_cond("True", BACKEND_REACHABLE_REASON)], 5),
            0
        );
        assert_eq!(timeout_streak(&[], 5), 0);
        // A negative streak clamps to zero.
        assert_eq!(
            timeout_streak(&[boot_false(BOOTSTRAP_DEADLINE_EXCEEDED_REASON)], -2),
            0
        );
    }

    /// The one `BootstrapFailure` → sensor-kind mapping (#414): only the
    /// genuine not-initialized sentinel reads Vanished, only a deadline kill
    /// reads TimedOut, everything else is the conservative Unreachable.
    #[test]
    fn probe_failure_kind_maps_each_failure_conservatively() {
        use crate::io::{BootstrapFailure, SeedFailure};
        use kopiur_kopia::KopiaErrorClass;
        assert_eq!(
            probe_failure_kind(&BootstrapFailure::RepositoryNotInitialized),
            ProbeFailureKind::Vanished
        );
        assert_eq!(
            probe_failure_kind(&BootstrapFailure::JobDeadlineExceeded {
                job_name: "j".into(),
                deadline_secs: 120,
            }),
            ProbeFailureKind::TimedOut
        );
        for failure in [
            BootstrapFailure::Backend {
                class: KopiaErrorClass::RepositoryUnavailable,
                message: "m".into(),
            },
            BootstrapFailure::JobFailedWithoutResult {
                job_name: "j".into(),
            },
            BootstrapFailure::Seed {
                failure: SeedFailure::SourceNotFound,
                message: "m".into(),
            },
            BootstrapFailure::SeedMoverTooOld,
            BootstrapFailure::InternalInconsistency {
                message: "m".into(),
            },
        ] {
            assert_eq!(
                probe_failure_kind(&failure),
                ProbeFailureKind::Unreachable,
                "{failure:?} must read as the conservative Unreachable"
            );
        }
    }

    /// #413 maintenance deadlock: Degraded-because-slow must not withhold the
    /// cure. The full gate truth table — phase × bootstrapped-before ×
    /// `BackendReachable` shape. The deny-list is exactly
    /// {BackendUnreachable, RepositoryVanished}: every other reason (a
    /// deadline-killed probe, a future reason this build has never seen)
    /// passes through as "maintenance may proceed" by construction.
    #[test]
    fn maintenance_gate_truth_table() {
        let br_false = |reason: &str| vec![reachable_cond("False", reason)];
        let br_true = || vec![reachable_cond("True", BACKEND_REACHABLE_REASON)];
        let uid = Some("uid-1");

        // Ready proceeds regardless of uniqueId or conditions.
        assert!(maintenance_may_proceed(
            Some(&RepositoryPhase::Ready),
            None,
            &[]
        ));
        assert!(maintenance_may_proceed(
            Some(&RepositoryPhase::Ready),
            uid,
            &br_false(BACKEND_UNREACHABLE_REASON)
        ));

        // Degraded + bootstrapped: proceeds unless confirmed down.
        let degraded = Some(&RepositoryPhase::Degraded);
        assert!(maintenance_may_proceed(degraded, uid, &[]));
        assert!(maintenance_may_proceed(degraded, uid, &br_true()));
        // The deadline-kill degradation (#414's reason) is NOT confirmation of
        // an outage — maintenance runs; so does any future unlisted reason.
        assert!(maintenance_may_proceed(
            degraded,
            uid,
            &br_false(PROBE_DEADLINE_EXCEEDED_REASON)
        ));
        assert!(maintenance_may_proceed(
            degraded,
            uid,
            &br_false("SomeFutureReason")
        ));
        // Confirmed unreachable / vanished: deferred (#345 breaker semantics).
        assert!(!maintenance_may_proceed(
            degraded,
            uid,
            &br_false(BACKEND_UNREACHABLE_REASON)
        ));
        assert!(!maintenance_may_proceed(
            degraded,
            uid,
            &br_false(REPOSITORY_VANISHED_REASON)
        ));
        // Degraded but never bootstrapped: nothing to maintain, deferred.
        assert!(!maintenance_may_proceed(degraded, None, &[]));

        // Every other phase defers, bootstrapped or not.
        for phase in [
            None,
            Some(&RepositoryPhase::Pending),
            Some(&RepositoryPhase::Initializing),
            Some(&RepositoryPhase::Failed),
        ] {
            assert!(
                !maintenance_may_proceed(phase, uid, &[]),
                "phase {phase:?} must defer"
            );
            assert!(!maintenance_may_proceed(phase, None, &[]));
        }
        let upgrading = RepositoryPhase::Unknown("Upgrading".into());
        assert!(
            !maintenance_may_proceed(Some(&upgrading), uid, &[]),
            "an unknown (newer-controller) phase must hold, not proceed"
        );
    }
}
