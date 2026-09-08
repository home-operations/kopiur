//! `GET /api/v1/schedules` — `SnapshotSchedule` rows with their next and last
//! fire times.
//!
//! A schedule fires a policy either by naming it (`policyRef`) or by selecting
//! it (`policySelector`). Both are shown, and [`fires_policy`] is the one place
//! the two are reconciled — for *display* only: which policies a selector
//! actually fires is the controller's decision, made against live labels, and
//! the UI never re-derives an authorization or a schedule from it.

use axum::extract::{Query, State};
use axum::{Json, Router, routing::get};

use kopiur_api::expand::label_selector_string;
use kopiur_api::{SnapshotPolicy, SnapshotSchedule};
use kopiur_ui_model::views::ScheduleRow;

use crate::AppState;
use crate::api::problem::ApiError;
use crate::api::{NamespaceQuery, client_for};
use crate::auth::CurrentIdentity;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new().route("/schedules", get(list))
}

/// **Pure.** One schedule as a table row.
pub fn schedule_row(s: &SnapshotSchedule) -> ScheduleRow {
    let status = s.status.as_ref();
    ScheduleRow {
        namespace: s.metadata.namespace.clone().unwrap_or_default(),
        name: s.metadata.name.clone().unwrap_or_default(),
        policy: s.spec.policy_ref.as_ref().map(|p| p.name.clone()),
        policy_selector: s.spec.policy_selector.as_ref().map(label_selector_string),
        // The cron is shown exactly as written, `H` token and all: the resolved
        // slot is deterministic but per-schedule, and rewriting it here would
        // show the user something they cannot find in their own manifest.
        cron: s.spec.schedule.cron.clone(),
        timezone: s.spec.schedule.timezone.clone(),
        suspended: s.spec.schedule.suspend,
        last_fire: status
            .and_then(|st| st.last_schedule.as_ref())
            .and_then(|r| r.at.clone()),
        next_fire: status
            .and_then(|st| st.next_schedule.as_ref())
            .and_then(|r| r.at.clone()),
        last_snapshot: status
            .and_then(|st| st.last_schedule.as_ref())
            .and_then(|r| r.snapshot_ref.as_ref())
            .map(|s| s.name.clone()),
        consecutive_failures: status.and_then(|st| st.consecutive_failures).unwrap_or(0),
    }
}

/// **Pure.** Whether this schedule would fire `policy`, for display.
///
/// A `policyRef` matches by name in the schedule's own namespace — the CRD gives
/// a schedule no way to fire a policy elsewhere. A `policySelector` matches the
/// policy's labels, evaluated here the same way
/// [`label_selector_string`] renders it: `matchLabels` only, because the
/// expression forms need an evaluator the API crate does not export and a
/// half-evaluated selector would silently *under*-report which schedules touch a
/// recipe. A schedule with `matchExpressions` therefore shows up on every policy
/// in its namespace whose `matchLabels` half agrees, which errs toward showing
/// the user a schedule that might fire rather than hiding one that does.
pub fn fires_policy(schedule: &SnapshotSchedule, policy: &SnapshotPolicy) -> bool {
    if schedule.metadata.namespace != policy.metadata.namespace {
        return false;
    }
    if let Some(policy_ref) = &schedule.spec.policy_ref {
        return policy_ref.name == policy.metadata.name.clone().unwrap_or_default();
    }
    let Some(selector) = &schedule.spec.policy_selector else {
        return false;
    };
    let labels = policy.metadata.labels.clone().unwrap_or_default();
    selector
        .match_labels
        .as_ref()
        .is_none_or(|want| want.iter().all(|(k, v)| labels.get(k) == Some(v)))
}

/// `GET /api/v1/schedules?namespace=`
async fn list(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    Query(q): Query<NamespaceQuery>,
) -> Result<Json<Vec<ScheduleRow>>, ApiError> {
    let client = client_for(&app, &id)?;
    let items = app
        .source
        .list::<SnapshotSchedule>(&id, &client, q.namespace.as_deref())
        .await?;
    let mut rows: Vec<ScheduleRow> = items.iter().map(|s| schedule_row(s)).collect();
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(Json(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::testutil::from_yaml;

    #[test]
    fn a_schedule_row_shows_the_cron_as_written_and_both_fire_times() {
        let s: SnapshotSchedule = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotSchedule
metadata: { name: nightly-cron, namespace: media }
spec:
  policyRef: { name: nightly }
  schedule: { cron: "H 2 * * *", timezone: "Europe/Berlin" }
status:
  lastSchedule: { at: "2026-09-08T02:17:00Z", snapshotRef: { name: nightly-20260908 } }
  nextSchedule: { at: "2026-09-09T02:17:00Z" }
  consecutiveFailures: 1
"#,
        );
        let row = schedule_row(&s);
        assert_eq!(row.namespace, "media");
        assert_eq!(row.name, "nightly-cron");
        assert_eq!(row.policy.as_deref(), Some("nightly"));
        assert_eq!(row.policy_selector, None);
        assert_eq!(
            row.cron, "H 2 * * *",
            "the jitter token stays as the user wrote it"
        );
        assert_eq!(row.timezone.as_deref(), Some("Europe/Berlin"));
        assert!(!row.suspended);
        assert_eq!(row.last_fire.as_deref(), Some("2026-09-08T02:17:00Z"));
        assert_eq!(row.next_fire.as_deref(), Some("2026-09-09T02:17:00Z"));
        assert_eq!(row.last_snapshot.as_deref(), Some("nightly-20260908"));
        assert_eq!(row.consecutive_failures, 1);
    }

    #[test]
    fn a_selector_schedule_renders_its_selector_and_no_policy_name() {
        let s: SnapshotSchedule = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotSchedule
metadata: { name: gold-cron, namespace: media }
spec:
  policySelector: { matchLabels: { tier: gold } }
  schedule: { cron: "0 3 * * *", suspend: true }
"#,
        );
        let row = schedule_row(&s);
        assert_eq!(row.policy, None);
        assert_eq!(row.policy_selector.as_deref(), Some("tier=gold"));
        assert!(row.suspended, "suspension lives on spec.schedule");
        assert_eq!(row.consecutive_failures, 0);
    }

    #[test]
    fn fires_policy_matches_by_reference_and_by_labels_within_one_namespace() {
        let gold: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: media, labels: { tier: gold, app: media } }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
"#,
        );
        let silver: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: weekly, namespace: media, labels: { tier: silver } }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
"#,
        );
        let elsewhere: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: apps, labels: { tier: gold } }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
"#,
        );

        let by_ref: SnapshotSchedule = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotSchedule
metadata: { name: nightly-cron, namespace: media }
spec:
  policyRef: { name: nightly }
  schedule: { cron: "0 2 * * *" }
"#,
        );
        let by_selector: SnapshotSchedule = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotSchedule
metadata: { name: gold-cron, namespace: media }
spec:
  policySelector: { matchLabels: { tier: gold } }
  schedule: { cron: "0 3 * * *" }
"#,
        );

        assert!(fires_policy(&by_ref, &gold));
        assert!(!fires_policy(&by_ref, &silver));
        assert!(
            !fires_policy(&by_ref, &elsewhere),
            "a schedule cannot fire a policy in another namespace"
        );
        assert!(fires_policy(&by_selector, &gold));
        assert!(!fires_policy(&by_selector, &silver));
        assert!(!fires_policy(&by_selector, &elsewhere));
    }

    #[test]
    fn an_empty_selector_fires_every_policy_in_its_namespace() {
        // `matchLabels: {}` is the "select everything" selector Kubernetes
        // semantics give it, and the display must agree.
        let everything: SnapshotSchedule = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotSchedule
metadata: { name: all-cron, namespace: media }
spec:
  policySelector: {}
  schedule: { cron: "0 1 * * *" }
"#,
        );
        let unlabelled: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: plain, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
"#,
        );
        assert!(fires_policy(&everything, &unlabelled));
    }

    #[test]
    fn a_schedule_that_names_neither_fires_nothing() {
        let neither: SnapshotSchedule = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotSchedule
metadata: { name: broken, namespace: media }
spec:
  schedule: { cron: "0 1 * * *" }
"#,
        );
        let policy: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: plain, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
"#,
        );
        assert!(!fires_policy(&neither, &policy));
    }
}
