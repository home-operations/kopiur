//! The `Restore` endpoints: the restore list and per-restore detail.
//!
//! The list answers [`RestoreRow`]; the detail answers [`RestoreDetail`], which
//! wraps that row and adds what a table has no room for — the conditions, the
//! source as it was *resolved and pinned* (which is what the run will read,
//! whatever the spec now says), the PVC actually being written to, the failure
//! block, and the redacted log tail.
//!
//! # Redaction is at the edge, as it is for snapshots
//!
//! `status.logTail` and `status.failure.message` are kopia's own output and can
//! carry a presigned URL or a token echoed back in an error, so both go through
//! [`crate::auth::redact::redact_text`] on the way out. `failure_view` is the
//! *snapshot* module's, reused rather than re-written: two failure renderings
//! would be two places to forget the redaction.

use axum::extract::State;
use axum::{Json, Router, routing::get};

use kopiur_api::Restore;
use kopiur_api::restore::ResolutionOutcome;
use kopiur_ui_model::views::{
    RestoreClaimView, RestoreDetail, RestoreRow, RestoreSourceView, RestoreTargetView,
    SnapshotRefView,
};

use crate::AppState;
use crate::api::problem::{ApiError, problem};
use crate::api::snapshots::failure_view;
use crate::api::{
    NamespaceQuery, UiPath, UiQuery, client_for, conditions_view, repo_ref_display,
    restore_claim_phase_label, restore_phase_view,
};
use crate::auth::CurrentIdentity;
use crate::auth::redact::redact_text;

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

/// **Pure.** Everything the detail screen shows.
pub fn view_detail(r: &Restore) -> RestoreDetail {
    let status = r.status.as_ref();
    RestoreDetail {
        row: restore_row(r),
        source: status.and_then(|s| s.resolved.as_ref()).map(|resolved| {
            let owner_ns = r.metadata.namespace.as_deref().unwrap_or_default();
            RestoreSourceView {
                resolution: resolved.resolution.as_ref().map(resolution_label),
                // An absent ref namespace resolves against the Restore's own,
                // the way every other reference in this API does — so the SPA
                // gets a link it can follow rather than half a coordinate.
                snapshot: resolved.snapshot_ref.as_ref().map(|s| SnapshotRefView {
                    namespace: s.namespace.clone().unwrap_or_else(|| owner_ns.to_string()),
                    name: s.name.clone(),
                }),
                pinned_at: resolved.pinned_at.clone(),
                identity: resolved
                    .identity
                    .as_ref()
                    .map(kopiur_api::identity::identity_string),
            }
        }),
        target: status
            .and_then(|s| s.target.as_ref())
            .map(|t| RestoreTargetView {
                pvc: t.pvc_ref.as_ref().map(|p| p.name.clone()),
                pvc_prime: t.pvc_prime.clone(),
            }),
        conditions: conditions_view(status.map(|s| s.conditions.as_slice()).unwrap_or_default()),
        failure: status.and_then(|s| s.failure.as_ref()).map(failure_view),
        log_tail: status
            .and_then(|s| s.log_tail.as_deref())
            .map(|t| t.lines().map(redact_text).collect())
            .unwrap_or_default(),
    }
}

/// **Pure.** The display string for a pinned resolution outcome. Exhaustive, so
/// a third outcome cannot ship without the detail screen naming it.
fn resolution_label(outcome: &ResolutionOutcome) -> String {
    match outcome {
        ResolutionOutcome::Snapshot => "Snapshot",
        ResolutionOutcome::NoSnapshot => "NoSnapshot",
    }
    .to_string()
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
    UiQuery(q): UiQuery<NamespaceQuery>,
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
    UiPath((namespace, name)): UiPath<(String, String)>,
) -> Result<Json<RestoreDetail>, ApiError> {
    let client = client_for(&app, &id)?;
    let restore = app
        .source
        .get::<Restore>(&id, &client, Some(&namespace), &name)
        .await?
        .ok_or_else(|| not_found(&namespace, &name))?;
    Ok(Json(view_detail(&restore)))
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

    /// The detail carries what the row cannot: the pinned resolution, the PVC
    /// actually being written to, and the conditions. Each field is asserted
    /// against the status path its doc names.
    #[test]
    fn the_detail_adds_the_resolved_source_target_and_conditions_the_row_omits() {
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
  resolved:
    resolution: Snapshot
    kopiaSnapshotID: k123
    snapshotRef: { name: nightly-1 }
    pinnedAt: "2026-09-08T11:59:00Z"
    identity: { username: kopiur, hostname: media, sourcePath: /data }
    repository: { kind: Repository, name: nas, namespace: media }
  target:
    pvcRef: { name: data }
    pvcPrime: prime-u1
  conditions:
    - type: Ready
      status: "False"
      reason: Restoring
      message: "the mover is running"
      lastTransitionTime: "2026-09-08T12:00:00Z"
"#,
        );
        let detail = view_detail(&r);

        assert_eq!(detail.row.name, "recover-db", "the row rides along whole");

        let source = detail.source.expect("status.resolved is set");
        assert_eq!(source.resolution.as_deref(), Some("Snapshot"));
        let snapshot = source.snapshot.expect("a resolved snapshotRef");
        assert_eq!(snapshot.name, "nightly-1");
        assert_eq!(
            snapshot.namespace, "media",
            "an absent ref namespace resolves against the Restore's own, so the \
             SPA gets a link it can follow"
        );
        assert_eq!(source.pinned_at.as_deref(), Some("2026-09-08T11:59:00Z"));
        assert_eq!(source.identity.as_deref(), Some("kopiur@media:/data"));

        let target = detail.target.expect("status.target is set");
        assert_eq!(target.pvc.as_deref(), Some("data"));
        assert_eq!(target.pvc_prime.as_deref(), Some("prime-u1"));

        assert_eq!(detail.conditions.len(), 1);
        assert_eq!(detail.conditions[0].r#type, "Ready");
        assert_eq!(detail.conditions[0].reason.as_deref(), Some("Restoring"));

        assert!(detail.failure.is_none(), "the run has not failed");
        assert!(detail.log_tail.is_empty(), "no log was written");
    }

    /// A restore that resolved to NO snapshot is a *successful* restore of an
    /// empty volume under `onMissingSnapshot: Continue`, not a failure — so the
    /// detail must be able to say which of the two outcomes was pinned.
    #[test]
    fn a_restore_that_matched_no_snapshot_says_so_rather_than_looking_unresolved() {
        let r: Restore = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Restore
metadata: { name: deploy-or-restore, namespace: media }
spec:
  source: { fromPolicy: { name: nightly } }
  target: { populator: {} }
status:
  phase: Completed
  resolved: { resolution: NoSnapshot }
"#,
        );
        let detail = view_detail(&r);
        let source = detail.source.expect("resolution was pinned");
        assert_eq!(source.resolution.as_deref(), Some("NoSnapshot"));
        assert!(source.snapshot.is_none(), "there was nothing to point at");
        assert!(source.identity.is_none());
    }

    /// Same rule as the snapshot detail: kopia's own text reaches the browser
    /// only through `redact_text`.
    #[test]
    fn the_restore_log_tail_and_failure_message_are_redacted() {
        let leaky: Restore = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Restore
metadata: { name: broken, namespace: media }
spec:
  source: { snapshotRef: { name: nightly-1 } }
  target: { pvcRef: { name: data } }
status:
  phase: Failed
  logTail: "connecting\nAWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI rejected"
  failure:
    kopiaErrorClass: RepositoryUnreachable
    message: "auth failed: AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI"
    retryRecommended: false
    exitCode: 1
    op: "repository connect"
"#,
        );
        let detail = view_detail(&leaky);
        let joined = detail.log_tail.join("\n");
        assert!(
            !joined.contains("wJalrXUtnFEMI"),
            "a credential quoted back by kopia must not reach the browser: {joined}"
        );
        let failure = detail.failure.expect("the run failed");
        assert!(
            !failure
                .message
                .unwrap_or_default()
                .contains("wJalrXUtnFEMI"),
            "the failure message is redacted too"
        );
        assert_eq!(failure.op.as_deref(), Some("repository connect"));
        assert_eq!(failure.retry_recommended, Some(false));
    }

    /// A fresh restore has none of it, and that is `None`/empty rather than a
    /// fabricated placeholder.
    #[test]
    fn an_unresolved_restore_has_no_source_target_or_conditions() {
        let fresh: Restore = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Restore
metadata: { name: fresh, namespace: media }
spec:
  source: { snapshotRef: { name: nightly-1 } }
  target: { pvcRef: { name: data } }
"#,
        );
        let detail = view_detail(&fresh);
        assert!(detail.source.is_none());
        assert!(detail.target.is_none());
        assert!(detail.conditions.is_empty());
        assert!(detail.failure.is_none());
        assert!(detail.log_tail.is_empty());
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
