//! Repository index-blob health (ADR-0005 §13).
//!
//! kopia's content index is a set of "index blobs" that periodic maintenance
//! compacts. When maintenance stops keeping up — most often because a stale
//! maintenance-lease owner makes every run yield (see
//! `crates/mover/src/main.rs::maintenance_restamp_target`) — the index-blob count
//! climbs unbounded and kopia eventually warns "Found too many index blobs (N)",
//! after which backups degrade.
//!
//! The bootstrap mover (and the in-process filesystem path) report the count via
//! `BootstrapResult.index_blob_count`. This module turns that count into a
//! non-blocking `IndexBlobHealth` condition + a one-shot Warning event when the
//! count crosses `spec.health.indexBlobWarnThreshold`. It is **informational**:
//! the repository stays `Ready`, so GitOps health gates are not tripped — too
//! many index blobs is a degradation, not an outage.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;

use crate::consts::INDEX_BLOB_HEALTH_CONDITION;
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
            let message = format!(
                "repository has {count} content-index blobs (threshold {threshold}); kopia \
                 maintenance is not compacting them. Ensure maintenance runs — if it is stuck on a \
                 stale lease owner, set spec.maintenance.takeoverPolicy: Force once to recover. \
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
}
