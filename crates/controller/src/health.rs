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

use kopiur_api::repository::RepositoryHealthStatus;

use crate::consts::{
    BACKEND_REACHABLE_CONDITION, BACKEND_REACHABLE_REASON, BACKEND_UNREACHABLE_REASON,
    CHECK_BACKEND_ACTION, INDEX_BLOB_HEALTH_CONDITION, REPOSITORY_VANISHED_REASON,
    VERIFY_BACKEND_ACTION,
};
use crate::io;

/// Machine reason on the `IndexBlobHealth` condition (and the Warning event) when
/// the count is over threshold.
pub const TOO_MANY_INDEX_BLOBS_REASON: &str = "TooManyIndexBlobs";
/// Remediation `action` carried on the Warning event.
pub const ENSURE_MAINTENANCE_ACTION: &str = "EnsureMaintenance";
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
                "repository has {count} content-index blobs (threshold {threshold}); kopia \
                 maintenance is not compacting them. Ensure maintenance runs — if it is stuck on a \
                 stale lease owner, set spec.maintenance.takeoverPolicy: Force once to recover. \
                 If maintenance IS running, the epoch-advance gate is the usual cause on a busy \
                 repository: an epoch cannot close before spec.parameters.epoch.minDuration \
                 (kopia's default is 24h) and compaction trails two epochs behind, so lowering \
                 it (e.g. 6h) lets blobs be compacted sooner. \
                 Raise spec.health.indexBlobWarnThreshold (or set it to 0) to silence this."
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
// Backend health probe (`spec.health.probe`) — opt-in, alert-only.
//
// Part A (always-on safety invariant) + Part B (the opt-in periodic probe).
// Both keep the repository `Ready`: a vanished/unreachable backend is surfaced
// as the `BackendReachable` condition + a Warning event, NEVER a phase flip
// (which would halt backups/replication) and NEVER an auto-recreate.
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

/// How a failing probe is classified — the load-bearing distinction the user
/// demanded before anything destructive is ever even *suggested*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailureKind {
    /// Backend reachable, kopia format blob absent (`RepositoryNotInitialized`) —
    /// a candidate *vanished* repository. Still only an alert.
    Vanished,
    /// Backend unreachable, mount/path missing, or auth/lock failed — NOT a
    /// confirmed wipe. kopiur never acts on it.
    Unreachable,
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
    })
}

/// Fold a **failing** probe into the health state, applying the consecutive-failure
/// debounce: the loud `BackendReachable=False` condition (and its Warning event)
/// is raised only once `failure_threshold` consecutive failures have accrued, so a
/// single transient blip never alarms or nudges a destructive manual recreate.
/// Phase stays `Ready` regardless — this is alert-only.
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
    let prior_failures = prior
        .and_then(|h| h.consecutive_probe_failures)
        .unwrap_or(0)
        .max(0);
    let consecutive = prior_failures + 1;
    // Continue the streak's first-failure stamp; start a fresh one if the prior
    // count was zero (a new episode).
    let first_failure_at = prior
        .and_then(|h| {
            if prior_failures > 0 {
                h.first_failure_at.clone()
            } else {
                None
            }
        })
        .unwrap_or_else(|| now.to_string());
    let health = RepositoryHealthStatus {
        last_probe_at: Some(now.to_string()),
        last_healthy_at: prior.and_then(|h| h.last_healthy_at.clone()),
        consecutive_probe_failures: Some(consecutive),
        first_failure_at: Some(first_failure_at),
    };

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
                "the kopia repository appears to have VANISHED: the backend is reachable but the \
                 repository format blob is absent ({consecutive} consecutive failing health \
                 probes). Data blobs may still remain — recreating would orphan them and destroy \
                 restorability, so kopiur will NOT auto-recreate. Verify the backend is truly \
                 empty (and that no other Repository/ClusterRepository points at the same backend) \
                 before any deliberate re-create."
            ),
        ),
        ProbeFailureKind::Unreachable => (
            BACKEND_UNREACHABLE_REASON,
            CHECK_BACKEND_ACTION,
            format!(
                "the repository backend could not be confirmed healthy ({consecutive} consecutive \
                 failing health probes): it is unreachable, the path/mount is missing, or \
                 credentials/lock failed. This is NOT treated as a wipe and kopiur takes no \
                 action. Check the backend, credentials, and any mounted volume."
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
    }
}
