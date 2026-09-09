//! `GET /api/v1/maintenance` — `Maintenance` rows, including the ones the
//! operator projects from a repository's `spec.maintenance`.
//!
//! # Managed and foreign look the same, and must not be confused
//!
//! Kopiur creates a `Maintenance` for a repository that asks for one, and honors
//! a hand-authored one it finds instead. The two are indistinguishable from
//! their spec — the difference is a controller `ownerReference` — so
//! [`MaintenanceRow::managed_by_repository`] is the field that tells a user
//! whether editing this object will stick or be reconciled away.

use axum::extract::State;
use axum::{Json, Router, routing::get};

use kopiur_api::Maintenance;
use kopiur_api::common::RepositoryKind;
use kopiur_api::consts::API_VERSION;
use kopiur_ui_model::views::{MaintenanceRow, ManualRunView, RunStatusView};

use crate::AppState;
use crate::api::problem::ApiError;
use crate::api::{NamespaceQuery, UiQuery, client_for, manual_run_phase_label, repo_ref_display};
use crate::auth::CurrentIdentity;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new().route("/maintenance", get(list))
}

/// **Pure.** One `Maintenance` as a table row.
pub fn maintenance_row(m: &Maintenance) -> MaintenanceRow {
    let namespace = m.metadata.namespace.clone().unwrap_or_default();
    let status = m.status.as_ref();
    let owner = repository_owner(m);
    MaintenanceRow {
        repository: repo_ref_display(&m.spec.repository, Some(&namespace)),
        namespace,
        name: m.metadata.name.clone().unwrap_or_default(),
        managed_by_repository: owner.is_some(),
        owner,
        quick: run_status_view(status.and_then(|s| s.quick.as_ref())),
        full: run_status_view(status.and_then(|s| s.full.as_ref())),
        manual_run: status
            .and_then(|s| s.manual_run.as_ref())
            .map(manual_run_view),
    }
}

/// **Pure.** The repository that *owns* this `Maintenance`, as `Kind/name`, or
/// `None` for a hand-authored one.
///
/// Ownership is the controller `ownerReference` back to a kopiur repository —
/// the same test `kopiur_controller::io::maintenance::is_managed_by` makes. The
/// `apiVersion` is checked too, so a `Repository` from an unrelated API group
/// cannot pass for kopiur's.
fn repository_owner(m: &Maintenance) -> Option<String> {
    m.metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|o| {
            o.controller == Some(true)
                && o.api_version == API_VERSION
                && (o.kind == RepositoryKind::Repository.kind_str()
                    || o.kind == RepositoryKind::ClusterRepository.kind_str())
        })
        .map(|o| format!("{}/{}", o.kind, o.name))
}

/// **Pure.** One maintenance track's history.
///
/// An absent track still renders — as a row with no runs and no failures —
/// because "quick maintenance has never run" is a fact the screen must show, not
/// a column to hide.
fn run_status_view(run: Option<&kopiur_api::maintenance::RunStatus>) -> RunStatusView {
    RunStatusView {
        last_run_at: run.and_then(|r| r.last_run_at.clone()),
        next_scheduled_at: run.and_then(|r| r.next_scheduled_at.clone()),
        consecutive_failures: run.and_then(|r| r.consecutive_failures).unwrap_or(0),
        last_content_reclaimed_bytes: run.and_then(|r| r.last_content_reclaimed_bytes),
    }
}

/// **Pure.** A user-requested run.
fn manual_run_view(run: &kopiur_api::maintenance::ManualRunStatus) -> ManualRunView {
    ManualRunView {
        requested_at: run.requested_at.clone(),
        mode: run.mode.map(|m| m.label().to_string()),
        phase: run.phase.as_ref().map(manual_run_phase_label),
        completed_at: run.completed_at.clone(),
    }
}

/// `GET /api/v1/maintenance?namespace=`
async fn list(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiQuery(q): UiQuery<NamespaceQuery>,
) -> Result<Json<Vec<MaintenanceRow>>, ApiError> {
    let client = client_for(&app, &id)?;
    let items = app
        .source
        .list::<Maintenance>(&id, &client, q.namespace.as_deref())
        .await?;
    let mut rows: Vec<MaintenanceRow> = items.iter().map(|m| maintenance_row(m)).collect();
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(Json(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::testutil::from_yaml;

    #[test]
    fn an_operator_owned_maintenance_names_its_repository_owner() {
        let managed: Maintenance = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Maintenance
metadata:
  name: nas-maintenance
  namespace: media
  ownerReferences:
    - apiVersion: kopiur.home-operations.com/v1alpha1
      kind: Repository
      name: nas
      uid: 0000-1111
      controller: true
spec:
  repository: { kind: Repository, name: nas }
  schedule: { quick: { cron: "0 * * * *" }, full: { cron: "0 4 * * 0" } }
  ownership: { owner: kopiur/media/nas }
status:
  quick: { lastRunAt: "2026-09-08T09:00:00Z", nextScheduledAt: "2026-09-08T10:00:00Z" }
  full: { consecutiveFailures: 2, lastContentReclaimedBytes: 4096 }
"#,
        );
        let row = maintenance_row(&managed);
        assert!(row.managed_by_repository);
        assert_eq!(row.owner.as_deref(), Some("Repository/nas"));
        assert_eq!(row.repository, "Repository/media/nas");
        assert_eq!(
            row.quick.last_run_at.as_deref(),
            Some("2026-09-08T09:00:00Z")
        );
        assert_eq!(row.quick.consecutive_failures, 0);
        assert_eq!(row.full.consecutive_failures, 2);
        assert_eq!(row.full.last_content_reclaimed_bytes, Some(4096));
        assert_eq!(row.manual_run, None);
    }

    #[test]
    fn a_hand_authored_maintenance_is_not_managed() {
        let foreign: Maintenance = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Maintenance
metadata: { name: mine, namespace: media }
spec:
  repository: { kind: ClusterRepository, name: shared }
  schedule: { quick: { cron: "0 * * * *" }, full: { cron: "0 4 * * 0" } }
  ownership: { owner: kopiur/clusterrepository/shared }
"#,
        );
        let row = maintenance_row(&foreign);
        assert!(
            !row.managed_by_repository,
            "the operator honors this one but never rewrites it"
        );
        assert_eq!(row.owner, None);
        assert_eq!(
            row.repository, "ClusterRepository/shared",
            "a cluster repository reference is namespace-free"
        );
        assert_eq!(
            row.quick.consecutive_failures, 0,
            "a track that has never run still renders"
        );
    }

    #[test]
    fn a_non_controller_owner_reference_does_not_make_it_managed() {
        // A plain (non-controller) ownerReference is a GC link, not the
        // operator's claim to author this object.
        let linked: Maintenance = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Maintenance
metadata:
  name: linked
  namespace: media
  ownerReferences:
    - apiVersion: kopiur.home-operations.com/v1alpha1
      kind: Repository
      name: nas
      uid: 0000-1111
spec:
  repository: { kind: Repository, name: nas }
  schedule: { quick: { cron: "0 * * * *" }, full: { cron: "0 4 * * 0" } }
  ownership: { owner: kopiur/media/nas }
"#,
        );
        assert!(!maintenance_row(&linked).managed_by_repository);
    }

    #[test]
    fn a_manual_run_reports_its_mode_and_phase_verbatim() {
        let requested: Maintenance = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Maintenance
metadata: { name: nas-maintenance, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  schedule: { quick: { cron: "0 * * * *" }, full: { cron: "0 4 * * 0" } }
  ownership: { owner: kopiur/media/nas }
status:
  manualRun:
    requestedAt: "2026-09-08T11:00:00Z"
    mode: full
    phase: Running
"#,
        );
        let run = maintenance_row(&requested)
            .manual_run
            .expect("a pending run shows");
        assert_eq!(run.requested_at.as_deref(), Some("2026-09-08T11:00:00Z"));
        assert_eq!(run.mode.as_deref(), Some("full"));
        assert_eq!(run.phase.as_deref(), Some("Running"));
        assert_eq!(run.completed_at, None);
    }

    #[test]
    fn a_manual_run_phase_from_a_newer_operator_is_carried_through() {
        let odd: Maintenance = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Maintenance
metadata: { name: nas-maintenance, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  schedule: { quick: { cron: "0 * * * *" }, full: { cron: "0 4 * * 0" } }
  ownership: { owner: kopiur/media/nas }
status:
  manualRun: { requestedAt: "2026-09-08T11:00:00Z", phase: Rechecking }
"#,
        );
        let run = maintenance_row(&odd).manual_run.unwrap();
        assert_eq!(
            run.phase.as_deref(),
            Some("Rechecking"),
            "an unrecognized phase is shown as written, never blanked"
        );
    }
}
