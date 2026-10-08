//! `GET /api/v1/overview` — how many objects of each kind are in scope, and
//! how many of them are in each health state.
//!
//! The overview's "fleet by kind" tiles. Every kind is listed through
//! [`Source::list`](crate::cache::Source::list), so the counts are exactly what
//! the caller may see. Only repositories publish a health of their own; every
//! other kind is bucketed here from its phase or its facts, by exhaustive
//! matches, so an unrecognised phase is `Unknown` and never healthy.

use std::sync::Arc;

use axum::extract::State;
use axum::{Json, Router, routing::get};
use chrono::{DateTime, Duration, Utc};

use kopiur_api::gates::GateScope;
use kopiur_api::{
    ClusterRepository, Maintenance, Repository, RepositoryReplication, RepositoryReplicationPhase,
    Restore, RestorePhase, Snapshot, SnapshotPhase, SnapshotPolicy, SnapshotReplication,
    SnapshotReplicationPhase, SnapshotSchedule,
};
use kopiur_ui_model::graph::{GateSeverityView, Health};
use kopiur_ui_model::views::{HealthCount, KindTally, ObjectKind, OverviewView};

use crate::AppState;
use crate::api::problem::ApiError;
use crate::api::repositories::{view_cluster_repository, view_repository};
use crate::api::{NamespaceQuery, UiQuery, client_for, gate_hits};
use crate::auth::CurrentIdentity;

/// Snapshots are counted over this many trailing hours.
pub const SNAPSHOT_WINDOW_HOURS: u32 = 24;

/// The order a tally lists its states in: worst first.
const WORST_FIRST: [Health; 6] = [
    Health::Failed,
    Health::Degraded,
    Health::Pending,
    Health::Unknown,
    Health::Suspended,
    Health::Healthy,
];

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new().route("/overview", get(handler))
}

/// **Pure.** A snapshot's state. `Discovered` is a backup kopiur did not make,
/// so it is not ours to call healthy.
pub fn snapshot_health(s: &Snapshot) -> Health {
    match s.status.as_ref().and_then(|st| st.phase.as_ref()) {
        Some(SnapshotPhase::Succeeded | SnapshotPhase::Unchanged) => Health::Healthy,
        Some(SnapshotPhase::Pending | SnapshotPhase::Running | SnapshotPhase::Deleting) => {
            Health::Pending
        }
        Some(SnapshotPhase::Failed) => Health::Failed,
        Some(SnapshotPhase::Discovered | SnapshotPhase::Unknown(_)) | None => Health::Unknown,
    }
}

/// **Pure.** A restore's state.
pub fn restore_health(r: &Restore) -> Health {
    match r.status.as_ref().and_then(|st| st.phase.as_ref()) {
        Some(RestorePhase::Completed) => Health::Healthy,
        Some(RestorePhase::Pending | RestorePhase::Resolving | RestorePhase::Restoring) => {
            Health::Pending
        }
        Some(RestorePhase::Failed) => Health::Failed,
        Some(RestorePhase::Unknown(_)) | None => Health::Unknown,
    }
}

/// **Pure.** A `RepositoryReplication`'s state.
pub fn repository_replication_health(phase: Option<&RepositoryReplicationPhase>) -> Health {
    match phase {
        Some(RepositoryReplicationPhase::Succeeded) => Health::Healthy,
        Some(RepositoryReplicationPhase::Pending | RepositoryReplicationPhase::Replicating) => {
            Health::Pending
        }
        Some(RepositoryReplicationPhase::Failed) => Health::Failed,
        Some(RepositoryReplicationPhase::Suspended) => Health::Suspended,
        Some(RepositoryReplicationPhase::Unknown(_)) | None => Health::Unknown,
    }
}

/// **Pure.** A `SnapshotReplication`'s state.
pub fn snapshot_replication_health(phase: Option<&SnapshotReplicationPhase>) -> Health {
    match phase {
        Some(SnapshotReplicationPhase::Succeeded) => Health::Healthy,
        Some(SnapshotReplicationPhase::Pending | SnapshotReplicationPhase::Replicating) => {
            Health::Pending
        }
        Some(SnapshotReplicationPhase::Failed) => Health::Failed,
        Some(SnapshotReplicationPhase::Suspended) => Health::Suspended,
        Some(SnapshotReplicationPhase::Unknown(_)) | None => Health::Unknown,
    }
}

/// **Pure.** A policy publishes no health: suspended, or failed when an
/// error-severity gate holds it, else healthy — the graph's policy rule.
pub fn policy_health(p: &SnapshotPolicy) -> Health {
    if p.spec.suspend {
        return Health::Suspended;
    }
    let conditions = p
        .status
        .as_ref()
        .map_or(&[][..], |s| s.conditions.as_slice());
    if gate_hits(conditions, GateScope::covers_snapshot_policy)
        .iter()
        .any(|g| g.severity == GateSeverityView::Error)
    {
        Health::Failed
    } else {
        Health::Healthy
    }
}

/// **Pure.** A schedule publishes no health: suspended, or failed while it has
/// consecutive failed runs, else healthy.
pub fn schedule_health(s: &SnapshotSchedule) -> Health {
    if s.spec.schedule.suspend {
        Health::Suspended
    } else if s
        .status
        .as_ref()
        .and_then(|st| st.consecutive_failures)
        .is_some_and(|n| n > 0)
    {
        Health::Failed
    } else {
        Health::Healthy
    }
}

/// **Pure.** Maintenance publishes no health: failed while either track has
/// consecutive failed runs, else healthy. A track that has never run is a loud
/// fact on its card, not a failure here — a new repository has not yet had
/// its first full run, and that is not something to fix.
pub fn maintenance_health(m: &Maintenance) -> Health {
    let failing = m.status.as_ref().is_some_and(|st| {
        [st.quick.as_ref(), st.full.as_ref()]
            .into_iter()
            .flatten()
            .any(|run| run.consecutive_failures.is_some_and(|n| n > 0))
    });
    if failing {
        Health::Failed
    } else {
        Health::Healthy
    }
}

/// **Pure.** One kind's tally: the total and the non-zero states, worst first.
pub fn kind_tally(kind: ObjectKind, states: impl IntoIterator<Item = Health>) -> KindTally {
    let states: Vec<Health> = states.into_iter().collect();
    let by_health = WORST_FIRST
        .iter()
        .filter_map(|h| {
            let count = states.iter().filter(|s| *s == h).count();
            (count > 0).then(|| HealthCount {
                health: h.clone(),
                count: u32::try_from(count).unwrap_or(u32::MAX),
            })
        })
        .collect();
    KindTally {
        kind,
        total: u32::try_from(states.len()).unwrap_or(u32::MAX),
        by_health,
    }
}

/// **Pure.** The snapshots whose run started (or, unstarted, were created)
/// within the last `hours` — the ops sort key, so "recent" means what the
/// snapshot list's newest-first order means.
pub fn within_window(
    snapshots: &[Arc<Snapshot>],
    now: DateTime<Utc>,
    hours: i64,
) -> impl Iterator<Item = &Arc<Snapshot>> {
    let since = now - Duration::hours(hours);
    snapshots
        .iter()
        .filter(move |s| kopiur_ops::snapshots::sort_key(s) >= since)
}

/// `GET /api/v1/overview?namespace=`
async fn handler(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiQuery(q): UiQuery<NamespaceQuery>,
) -> Result<Json<OverviewView>, ApiError> {
    let client = client_for(&app, &id)?;
    let ns = q.namespace.as_deref();
    let src = &app.source;
    let now = Utc::now();

    let repositories = src.list::<Repository>(&id, &client, ns).await?;
    let cluster_repositories = src.list::<ClusterRepository>(&id, &client, ns).await?;
    let maintenances = src.list::<Maintenance>(&id, &client, ns).await?;
    let policies = src.list::<SnapshotPolicy>(&id, &client, ns).await?;
    let schedules = src.list::<SnapshotSchedule>(&id, &client, ns).await?;
    let snapshots = src.list::<Snapshot>(&id, &client, ns).await?;
    let restores = src.list::<Restore>(&id, &client, ns).await?;
    let repository_replications = src.list::<RepositoryReplication>(&id, &client, ns).await?;
    let snapshot_replications = src.list::<SnapshotReplication>(&id, &client, ns).await?;

    let kinds = vec![
        kind_tally(
            ObjectKind::Repository,
            repositories.iter().map(|r| view_repository(r).health),
        ),
        kind_tally(
            ObjectKind::ClusterRepository,
            cluster_repositories
                .iter()
                .map(|r| view_cluster_repository(r).health),
        ),
        kind_tally(
            ObjectKind::Maintenance,
            maintenances.iter().map(|m| maintenance_health(m)),
        ),
        kind_tally(
            ObjectKind::SnapshotPolicy,
            policies.iter().map(|p| policy_health(p)),
        ),
        kind_tally(
            ObjectKind::SnapshotSchedule,
            schedules.iter().map(|s| schedule_health(s)),
        ),
        kind_tally(
            ObjectKind::Snapshot,
            within_window(&snapshots, now, i64::from(SNAPSHOT_WINDOW_HOURS))
                .map(|s| snapshot_health(s)),
        ),
        kind_tally(
            ObjectKind::Restore,
            restores.iter().map(|r| restore_health(r)),
        ),
        kind_tally(
            ObjectKind::RepositoryReplication,
            repository_replications.iter().map(|r| {
                repository_replication_health(r.status.as_ref().and_then(|st| st.phase.as_ref()))
            }),
        ),
        kind_tally(
            ObjectKind::SnapshotReplication,
            snapshot_replications.iter().map(|r| {
                snapshot_replication_health(r.status.as_ref().and_then(|st| st.phase.as_ref()))
            }),
        ),
    ];

    Ok(Json(OverviewView {
        kinds,
        snapshot_window_hours: SNAPSHOT_WINDOW_HOURS,
        generated_at: now.to_rfc3339(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::testutil::from_yaml;

    fn snap(name: &str, phase: &str, start: &str) -> Arc<Snapshot> {
        Arc::new(from_yaml(&format!(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: {{ name: {name}, namespace: media, creationTimestamp: "{start}" }}
spec: {{ policyRef: {{ name: nightly }} }}
status: {{ phase: {phase}, timing: {{ startTime: "{start}" }} }}
"#
        )))
    }

    #[test]
    fn snapshot_phases_bucket_into_health_and_an_unrecognised_one_is_unknown() {
        let at = "2026-10-08T10:00:00Z";
        assert_eq!(
            snapshot_health(&snap("a", "Succeeded", at)),
            Health::Healthy
        );
        assert_eq!(
            snapshot_health(&snap("b", "Unchanged", at)),
            Health::Healthy
        );
        assert_eq!(snapshot_health(&snap("c", "Running", at)), Health::Pending);
        assert_eq!(snapshot_health(&snap("d", "Failed", at)), Health::Failed);
        assert_eq!(
            snapshot_health(&snap("e", "Discovered", at)),
            Health::Unknown
        );
        assert_eq!(
            snapshot_health(&snap("f", "Archiving", at)),
            Health::Unknown,
            "a phase from a newer operator is never counted as healthy"
        );
    }

    #[test]
    fn restore_and_replication_phases_bucket_into_health() {
        let restore = |phase: &str| -> Restore {
            from_yaml(&format!(
                r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Restore
metadata: {{ name: r, namespace: media }}
spec: {{ source: {{ snapshotRef: {{ name: s }} }}, target: {{ pvcRef: {{ name: data }} }} }}
status: {{ phase: {phase} }}
"#
            ))
        };
        assert_eq!(restore_health(&restore("Completed")), Health::Healthy);
        assert_eq!(restore_health(&restore("Restoring")), Health::Pending);
        assert_eq!(restore_health(&restore("Failed")), Health::Failed);
        assert_eq!(restore_health(&restore("Teleporting")), Health::Unknown);

        use kopiur_api::{RepositoryReplicationPhase as R, SnapshotReplicationPhase as S};
        assert_eq!(
            repository_replication_health(Some(&R::Suspended)),
            Health::Suspended
        );
        assert_eq!(repository_replication_health(None), Health::Unknown);
        assert_eq!(
            snapshot_replication_health(Some(&S::Replicating)),
            Health::Pending
        );
        assert_eq!(
            snapshot_replication_health(Some(&S::Unknown("Mirroring".into()))),
            Health::Unknown
        );
    }

    #[test]
    fn policies_schedules_and_maintenance_bucket_on_their_facts() {
        let policy: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
  suspend: true
"#,
        );
        assert_eq!(policy_health(&policy), Health::Suspended);

        let schedule: SnapshotSchedule = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotSchedule
metadata: { name: nightly-cron, namespace: media }
spec: { policyRef: { name: nightly }, schedule: { cron: "0 2 * * *" } }
status: { consecutiveFailures: 2 }
"#,
        );
        assert_eq!(schedule_health(&schedule), Health::Failed);

        let maintenance: Maintenance = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Maintenance
metadata: { name: nas-maint, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  schedule: { quick: { cron: "0 * * * *" }, full: { cron: "0 4 * * 0" } }
  ownership: { owner: kopiur/media/nas }
status: { quick: { consecutiveFailures: 0 } }
"#,
        );
        assert_eq!(
            maintenance_health(&maintenance),
            Health::Healthy,
            "a full track that has never run is a loud card fact, not a tile failure"
        );
    }

    #[test]
    fn tally_orders_worst_first_and_omits_zero_buckets() {
        let t = kind_tally(
            ObjectKind::Snapshot,
            [
                Health::Healthy,
                Health::Failed,
                Health::Healthy,
                Health::Unknown,
            ],
        );
        assert_eq!(t.kind, ObjectKind::Snapshot);
        assert_eq!(t.total, 4);
        assert_eq!(
            t.by_health,
            vec![
                HealthCount {
                    health: Health::Failed,
                    count: 1
                },
                HealthCount {
                    health: Health::Unknown,
                    count: 1
                },
                HealthCount {
                    health: Health::Healthy,
                    count: 2
                },
            ]
        );
        assert_eq!(kind_tally(ObjectKind::Restore, []).total, 0);
    }

    #[test]
    fn snapshots_outside_the_window_are_not_counted() {
        let now: DateTime<Utc> = "2026-10-08T12:00:00Z".parse().unwrap();
        let all = vec![
            snap("recent", "Succeeded", "2026-10-08T10:00:00Z"),
            snap("old", "Succeeded", "2026-10-07T06:00:00Z"),
        ];
        let names: Vec<_> = within_window(&all, now, 24)
            .filter_map(|s| s.metadata.name.clone())
            .collect();
        assert_eq!(names, ["recent"]);
    }
}
