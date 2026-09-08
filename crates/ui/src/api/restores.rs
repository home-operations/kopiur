//! The `Restore` endpoints: the restore list and per-restore detail.
//!
//! Both answer the same [`RestoreRow`]. A restore's whole story fits in one row
//! — where it reads from, where it writes to, how far it has got, and per-claim
//! progress for a populator fan-out — so there is nothing a detail type would
//! add beyond a second shape to keep in step.

use axum::extract::{Path, Query, State};
use axum::{Json, Router, routing::get};

use kopiur_api::Restore;
use kopiur_ui_model::views::{RestoreClaimView, RestoreRow};

use crate::AppState;
use crate::api::problem::{ApiError, problem};
use crate::api::{
    NamespaceQuery, client_for, repo_ref_display, restore_claim_phase_label, restore_phase_view,
};
use crate::auth::CurrentIdentity;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/restores", get(list))
        .route("/restores/{namespace}/{name}", get(detail))
}

/// **Pure.** One restore as a table row.
pub fn restore_row(r: &Restore) -> RestoreRow {
    let namespace = r.metadata.namespace.clone().unwrap_or_default();
    let status = r.status.as_ref();
    let resolved = status.and_then(|s| s.resolved.as_ref());
    let timing = status.and_then(|s| s.timing.as_ref());
    let progress = status.and_then(|s| s.progress.as_ref());

    RestoreRow {
        name: r.metadata.name.clone().unwrap_or_default(),
        phase: status
            .and_then(|s| s.phase.as_ref())
            .map(restore_phase_view),
        // The pinned discriminant is preferred over the spec's: it is what the
        // run actually resolved, and it survives a spec edit.
        source_kind: status
            .and_then(|s| s.source_kind.clone())
            .or_else(|| Some(r.spec.source.kind_str().to_string())),
        target_kind: r.spec.target.kind_str().to_string(),
        repository: resolved
            .and_then(|res| res.repository.as_ref())
            .or(r.spec.repository.as_ref())
            .map(|rr| repo_ref_display(rr, Some(&namespace))),
        kopia_snapshot_id: resolved.and_then(|res| res.kopia_snapshot_id.clone()),
        start_time: timing.and_then(|t| t.start_time.clone()),
        end_time: timing.and_then(|t| t.end_time.clone()),
        bytes_restored: progress.and_then(|p| p.bytes_restored),
        files_restored: progress.and_then(|p| p.files_restored),
        claims: claims_view(r),
        namespace,
    }
}

/// **Pure.** Per-PVC progress for a populator restore.
///
/// Sorted by PVC name so a fan-out's rows do not reshuffle between polls — the
/// underlying `claims` map is a `BTreeMap`, but the ordering is load-bearing for
/// the UI and is asserted rather than assumed.
fn claims_view(r: &Restore) -> Vec<RestoreClaimView> {
    let Some(status) = r.status.as_ref() else {
        return Vec::new();
    };
    let mut claims: Vec<RestoreClaimView> = status
        .claims
        .iter()
        .map(|(pvc, claim)| RestoreClaimView {
            pvc: pvc.clone(),
            // An unobserved claim is reported as `Pending` rather than blank:
            // the claim exists, and "we have not looked yet" is a state.
            phase: claim
                .phase
                .as_ref()
                .map(restore_claim_phase_label)
                .unwrap_or_else(|| "Pending".to_string()),
            message: claim.message.clone().or_else(|| claim.reason.clone()),
        })
        .collect();
    claims.sort_by(|a, b| a.pvc.cmp(&b.pvc));
    claims
}

/// `GET /api/v1/restores?namespace=`
async fn list(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    Query(q): Query<NamespaceQuery>,
) -> Result<Json<Vec<RestoreRow>>, ApiError> {
    let client = client_for(&app, &id)?;
    let items = app
        .source
        .list::<Restore>(&id, &client, q.namespace.as_deref())
        .await?;
    let mut rows: Vec<RestoreRow> = items.iter().map(|r| restore_row(r)).collect();
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(Json(rows))
}

/// `GET /api/v1/restores/{namespace}/{name}`
async fn detail(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<RestoreRow>, ApiError> {
    let client = client_for(&app, &id)?;
    let restore = app
        .source
        .get::<Restore>(&id, &client, Some(&namespace), &name)
        .await?
        .ok_or_else(|| not_found(&namespace, &name))?;
    Ok(Json(restore_row(&restore)))
}

/// The 404 for a restore that is not there.
fn not_found(namespace: &str, name: &str) -> ApiError {
    problem(
        404,
        "not-found",
        format!("There is no Restore called {name} in namespace {namespace}."),
        "It was deleted or renamed — the SPA may be showing a link from a listing taken before \
         the change.",
        "reload the restores list to see what the cluster holds now",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::testutil::from_yaml;
    use kopiur_ui_model::views::RestorePhaseView;

    #[test]
    fn a_restore_row_shows_where_it_reads_and_writes_and_how_far_it_got() {
        let r: Restore = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Restore
metadata: { name: recover-db, namespace: media }
spec:
  source: { snapshotRef: { name: nightly-1 } }
  target: { pvcRef: { name: data } }
status:
  phase: Restoring
  sourceKind: SnapshotRef
  timing: { startTime: "2026-09-08T12:00:00Z" }
  progress: { bytesRestored: 4096, filesRestored: 12 }
  resolved:
    kopiaSnapshotID: k123
    repository: { kind: Repository, name: nas, namespace: media }
"#,
        );
        let row = restore_row(&r);
        assert_eq!(row.namespace, "media");
        assert_eq!(row.name, "recover-db");
        assert_eq!(row.phase, Some(RestorePhaseView::Restoring));
        assert_eq!(row.source_kind.as_deref(), Some("SnapshotRef"));
        assert_eq!(row.target_kind, "PvcRef");
        assert_eq!(row.repository.as_deref(), Some("Repository/media/nas"));
        assert_eq!(row.kopia_snapshot_id.as_deref(), Some("k123"));
        assert_eq!(row.bytes_restored, Some(4096));
        assert_eq!(row.files_restored, Some(12));
        assert_eq!(row.end_time, None);
        assert!(row.claims.is_empty(), "a direct restore has no claims");
    }

    #[test]
    fn an_unreconciled_restore_still_knows_its_own_source_and_target_kinds() {
        let fresh: Restore = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Restore
metadata: { name: fresh, namespace: media }
spec:
  source: { fromPolicy: { name: nightly } }
  target: { pvc: { name: restored } }
"#,
        );
        let row = restore_row(&fresh);
        assert_eq!(row.phase, None);
        assert_eq!(
            row.source_kind.as_deref(),
            Some("FromPolicy"),
            "before status.sourceKind is pinned, the spec answers"
        );
        assert_eq!(row.target_kind, "Pvc");
    }

    #[test]
    fn a_populator_restore_reports_one_row_per_claiming_pvc_in_a_stable_order() {
        let fanout: Restore = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Restore
metadata: { name: populate, namespace: media }
spec:
  repository: { kind: ClusterRepository, name: shared }
  source: { identity: { username: kopiur, hostname: media, sourcePath: /data } }
  target: { populator: {} }
status:
  phase: Restoring
  claims:
    zeta:
      phase: Populating
      reason: PopulatingPrimePvc
    alpha:
      phase: Failed
      reason: MoverJobFailed
      message: "the mover exited 1"
    beta: {}
"#,
        );
        let row = restore_row(&fanout);
        assert_eq!(row.target_kind, "Populator");
        assert_eq!(
            row.repository.as_deref(),
            Some("ClusterRepository/shared"),
            "with no resolved pin yet, the explicit spec repository answers"
        );
        let pvcs: Vec<&str> = row.claims.iter().map(|c| c.pvc.as_str()).collect();
        assert_eq!(pvcs, vec!["alpha", "beta", "zeta"], "sorted by PVC name");
        assert_eq!(row.claims[0].phase, "Failed");
        assert_eq!(
            row.claims[0].message.as_deref(),
            Some("the mover exited 1"),
            "a message wins over the machine-readable reason"
        );
        assert_eq!(
            row.claims[1].phase, "Pending",
            "an unobserved claim is pending, not blank"
        );
        assert_eq!(row.claims[1].message, None);
        assert_eq!(row.claims[2].phase, "Populating");
        assert_eq!(
            row.claims[2].message.as_deref(),
            Some("PopulatingPrimePvc"),
            "with no message, the reason is the next best thing to show"
        );
    }

    #[test]
    fn a_phase_this_build_does_not_know_is_carried_through_rather_than_blanked() {
        let odd: Restore = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Restore
metadata: { name: odd, namespace: media }
spec:
  source: { snapshotRef: { name: nightly-1 } }
  target: { pvcRef: { name: data } }
status:
  phase: Verifying
  claims:
    alpha: { phase: Staging }
"#,
        );
        let row = restore_row(&odd);
        assert_eq!(
            row.phase,
            Some(RestorePhaseView::Unknown {
                raw: "Verifying".into()
            })
        );
        assert_eq!(row.claims[0].phase, "Staging");
    }
}
