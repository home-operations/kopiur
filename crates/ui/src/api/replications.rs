//! `GET /api/v1/replications` — `RepositoryReplication` and `SnapshotReplication`
//! rows.
//!
//! The two kinds copy different things — one syncs a repository's blobs to a bare
//! backend, the other migrates selected snapshots between repository CRs — so
//! they have different rows and are returned side by side under one request
//! rather than forced into a shared shape that would fit neither.

use axum::extract::State;
use axum::{Json, Router, routing::get};

use kopiur_api::{RepositoryReplication, SnapshotReplication};
use kopiur_ui_model::views::{ReplicationsView, RepositoryReplicationRow, SnapshotReplicationRow};

use crate::AppState;
use crate::api::problem::ApiError;
use crate::api::{
    NamespaceQuery, UiQuery, client_for, repo_ref_display, repository_replication_phase_view,
    snapshot_replication_phase_view,
};
use crate::auth::CurrentIdentity;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new().route("/replications", get(list))
}

/// **Pure.** One `RepositoryReplication` as a table row.
pub fn repository_replication_row(r: &RepositoryReplication) -> RepositoryReplicationRow {
    let namespace = r.metadata.namespace.clone().unwrap_or_default();
    let status = r.status.as_ref();
    RepositoryReplicationRow {
        name: r.metadata.name.clone().unwrap_or_default(),
        source: repo_ref_display(&r.spec.source_ref, Some(&namespace)),
        destination_backend: status
            .and_then(|s| s.destination_backend.clone())
            .or_else(|| Some(r.spec.destination.kind_str().to_string())),
        cron: r.spec.schedule.cron.clone(),
        suspended: r.spec.suspend,
        phase: status
            .and_then(|s| s.phase.as_ref())
            .map(repository_replication_phase_view),
        last_replicated: status.and_then(|s| s.last_replicated.clone()),
        next_scheduled_at: status.and_then(|s| s.next_scheduled_at.clone()),
        last_replicated_bytes: status.and_then(|s| s.last_replicated_bytes),
        last_replicated_blobs: status.and_then(|s| s.last_replicated_blobs),
        namespace,
    }
}

/// **Pure.** One `SnapshotReplication` as a table row.
pub fn snapshot_replication_row(r: &SnapshotReplication) -> SnapshotReplicationRow {
    let namespace = r.metadata.namespace.clone().unwrap_or_default();
    let status = r.status.as_ref();
    let run = status.and_then(|s| s.last_run.as_ref());
    SnapshotReplicationRow {
        name: r.metadata.name.clone().unwrap_or_default(),
        source: repo_ref_display(&r.spec.source_ref, Some(&namespace)),
        destination: repo_ref_display(&r.spec.destination_ref, Some(&namespace)),
        cron: r.spec.schedule.cron.clone(),
        suspended: r.spec.suspend,
        phase: status
            .and_then(|s| s.phase.as_ref())
            .map(snapshot_replication_phase_view),
        last_replicated: status.and_then(|s| s.last_replicated.clone()),
        identities_selected: run.and_then(|s| s.identities_selected),
        snapshots_copied: run.and_then(|s| s.snapshots_copied),
        already_present: run.and_then(|s| s.already_present),
        failed: run.and_then(|s| s.failed),
        pruned: run.and_then(|s| s.pruned),
        namespace,
    }
}

/// `GET /api/v1/replications?namespace=`
async fn list(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiQuery(q): UiQuery<NamespaceQuery>,
) -> Result<Json<ReplicationsView>, ApiError> {
    let namespace = q.namespace.as_deref();
    let client = client_for(&app, &id)?;

    let repository_items = app
        .source
        .list::<RepositoryReplication>(&id, &client, namespace)
        .await?;
    let snapshot_items = app
        .source
        .list::<SnapshotReplication>(&id, &client, namespace)
        .await?;

    let mut repository: Vec<RepositoryReplicationRow> = repository_items
        .iter()
        .map(|r| repository_replication_row(r))
        .collect();
    repository.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));

    let mut snapshot: Vec<SnapshotReplicationRow> = snapshot_items
        .iter()
        .map(|r| snapshot_replication_row(r))
        .collect();
    snapshot.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));

    Ok(Json(ReplicationsView {
        repository,
        snapshot,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::testutil::from_yaml;
    use kopiur_ui_model::views::ReplicationPhaseView;

    #[test]
    fn a_repository_replication_row_names_its_backend_and_last_run() {
        let r: RepositoryReplication = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: RepositoryReplication
metadata: { name: blobsync, namespace: media }
spec:
  sourceRef: { kind: Repository, name: nas }
  destination: { s3: { bucket: dr-bucket } }
  schedule: { cron: "0 5 * * *" }
status:
  phase: Succeeded
  destinationBackend: S3
  lastReplicated: "2026-09-08T05:12:00Z"
  nextScheduledAt: "2026-09-09T05:00:00Z"
  lastReplicatedBytes: 1048576
  lastReplicatedBlobs: 42
"#,
        );
        let row = repository_replication_row(&r);
        assert_eq!(row.namespace, "media");
        assert_eq!(row.name, "blobsync");
        assert_eq!(row.source, "Repository/media/nas");
        assert_eq!(row.destination_backend.as_deref(), Some("S3"));
        assert_eq!(row.cron, "0 5 * * *");
        assert!(!row.suspended);
        assert_eq!(row.phase, Some(ReplicationPhaseView::Succeeded));
        assert_eq!(row.last_replicated_bytes, Some(1_048_576));
        assert_eq!(row.last_replicated_blobs, Some(42));
    }

    #[test]
    fn an_unreconciled_repository_replication_reads_its_backend_from_the_spec() {
        let r: RepositoryReplication = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: RepositoryReplication
metadata: { name: fresh, namespace: media }
spec:
  sourceRef: { kind: ClusterRepository, name: shared }
  destination: { filesystem: { path: /mirror } }
  schedule: { cron: "0 5 * * *" }
  suspend: true
"#,
        );
        let row = repository_replication_row(&r);
        assert_eq!(row.source, "ClusterRepository/shared");
        assert_eq!(row.destination_backend.as_deref(), Some("Filesystem"));
        assert!(row.suspended);
        assert_eq!(row.phase, None);
    }

    #[test]
    fn a_snapshot_replication_row_carries_its_last_runs_counters() {
        let r: SnapshotReplication = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotReplication
metadata: { name: offsite, namespace: media }
spec:
  sourceRef: { kind: Repository, name: nas }
  destinationRef: { kind: ClusterRepository, name: shared }
  schedule: { cron: "0 3 * * *" }
status:
  phase: Replicating
  lastReplicated: "2026-09-08T03:10:00Z"
  lastRun:
    identitiesSelected: 3
    snapshotsCopied: 7
    alreadyPresent: 120
    failed: 1
    pruned: 2
"#,
        );
        let row = snapshot_replication_row(&r);
        assert_eq!(row.source, "Repository/media/nas");
        assert_eq!(row.destination, "ClusterRepository/shared");
        assert_eq!(row.phase, Some(ReplicationPhaseView::Replicating));
        assert_eq!(row.identities_selected, Some(3));
        assert_eq!(row.snapshots_copied, Some(7));
        assert_eq!(row.already_present, Some(120));
        assert_eq!(row.failed, Some(1));
        assert_eq!(row.pruned, Some(2));
    }

    #[test]
    fn a_replication_phase_from_a_newer_operator_degrades_to_a_labelled_chip() {
        let r: SnapshotReplication = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotReplication
metadata: { name: odd, namespace: media }
spec:
  sourceRef: { kind: Repository, name: nas }
  destinationRef: { kind: ClusterRepository, name: shared }
  schedule: { cron: "0 3 * * *" }
status: { phase: Reconciling }
"#,
        );
        assert_eq!(
            snapshot_replication_row(&r).phase,
            Some(ReplicationPhaseView::Unknown {
                raw: "Reconciling".into()
            })
        );
    }

    #[test]
    fn the_envelope_keys_are_the_two_kinds() {
        let body = serde_json::to_value(ReplicationsView {
            repository: Vec::new(),
            snapshot: Vec::new(),
        })
        .unwrap();
        assert!(body.get("repository").is_some(), "got {body}");
        assert!(body.get("snapshot").is_some(), "got {body}");
    }
}
