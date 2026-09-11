//! The `SnapshotPolicy` endpoints: the policy list and per-policy detail.
//!
//! A policy is the *recipe*; a `Snapshot` is one invocation of it and a
//! `SnapshotSchedule` is what fires it. The detail screen therefore joins in
//! both directions — the schedules that would fire this recipe and the snapshots
//! it has produced — because neither is reachable from the policy object alone.

use std::sync::Arc;

use axum::extract::State;
use axum::{Json, Router, routing::get};

use kopiur_api::common::repo_key;
use kopiur_api::gates::GateScope;
use kopiur_api::snapshot_policy::{is_multi_repo, repository_refs};
use kopiur_api::{Snapshot, SnapshotPolicy, SnapshotSchedule};
use kopiur_ops::snapshots::sort_key;
use kopiur_ui_model::views::{
    PolicyDetail, PolicyRow, RepoVerificationView, RetentionView, SnapshotRow,
};

use crate::AppState;
use crate::api::problem::{ApiError, problem};
use crate::api::schedules::{fires_policy, schedule_row};
use crate::api::snapshots::view_row;
use crate::api::{
    NamespaceQuery, UiPath, UiQuery, client_for, conditions_view, gate_hits, repo_ref_display,
};
use crate::auth::CurrentIdentity;
use kopiur_ops::snapshots::policy_of;

/// How many recent runs the detail screen shows.
const RECENT_SNAPSHOTS: usize = 10;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/policies", get(list))
        .route("/policies/{namespace}/{name}", get(detail))
}

/// **Pure.** One policy as a table row.
pub fn view_row_of(policy: &SnapshotPolicy) -> PolicyRow {
    let namespace = policy.metadata.namespace.clone().unwrap_or_default();
    let status = policy.status.as_ref();
    PolicyRow {
        name: policy.metadata.name.clone().unwrap_or_default(),
        repositories: repository_refs(&policy.spec)
            .map(|r| repo_ref_display(r, Some(&namespace)))
            .collect(),
        multi_repo: is_multi_repo(&policy.spec),
        suspended: policy.spec.suspend,
        last_successful_snapshot: status.and_then(|s| s.last_successful_snapshot.clone()),
        last_verified: status.and_then(|s| s.last_verified.clone()),
        active_snapshot_count: status
            .and_then(|s| s.retention.as_ref())
            .and_then(|r| r.active_snapshot_count),
        namespace,
    }
}

/// **Pure.** `spec.retention` as the wire view.
///
/// The CRD's counts are `u32` and the wire's are `i32`: both are bounded well
/// below either type's limit by the schema, and the wire keeps the signed type
/// TypeScript actually receives.
fn retention_view(r: &kopiur_api::common::Retention) -> RetentionView {
    RetentionView {
        keep_latest: r.keep_latest.map(|v| v as i32),
        keep_hourly: r.keep_hourly.map(|v| v as i32),
        keep_daily: r.keep_daily.map(|v| v as i32),
        keep_weekly: r.keep_weekly.map(|v| v as i32),
        keep_monthly: r.keep_monthly.map(|v| v as i32),
        keep_annual: r.keep_annual.map(|v| v as i32),
    }
}

/// **Pure.** Per-repository verification state.
///
/// A multi-repository policy records one entry per member; a single-repository
/// one records the flat `status.lastVerified`, which is projected onto the one
/// repository it writes to so the screen has a single shape to render.
fn verification_view(policy: &SnapshotPolicy) -> Vec<RepoVerificationView> {
    let namespace = policy.metadata.namespace.clone().unwrap_or_default();
    let status = policy.status.as_ref();
    let per_repo = status
        .map(|s| s.verification.as_slice())
        .unwrap_or_default();
    if !per_repo.is_empty() {
        return per_repo
            .iter()
            .map(|v| RepoVerificationView {
                repository: repo_ref_display(&v.repository, Some(&namespace)),
                last_verified: v.last_verified.clone(),
            })
            .collect();
    }
    repository_refs(&policy.spec)
        .map(|r| RepoVerificationView {
            repository: repo_ref_display(r, Some(&namespace)),
            last_verified: status.and_then(|s| s.last_verified.clone()),
        })
        .collect()
}

/// **Pure.** The source paths this recipe covers.
///
/// The resolved sources are preferred — they are what the last run actually
/// backed up — with the spec as the fallback for a recipe that has not run.
fn sources_view(policy: &SnapshotPolicy) -> Vec<String> {
    let resolved: Vec<String> = policy
        .status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .map(|r| {
            r.sources
                .iter()
                .filter_map(|s| s.source_path.clone().or_else(|| s.pvc.clone()))
                .collect()
        })
        .unwrap_or_default();
    if !resolved.is_empty() {
        return resolved;
    }
    policy.spec.sources.iter().map(describe_source).collect()
}

/// **Pure.** A declared source, before expansion, in the terms the user wrote it.
///
/// A `pvcSelector` is rendered as the selector rather than as the PVCs it
/// matches: enumerating them needs a `persistentvolumeclaims` LIST the read API
/// does not perform, and showing an empty list would read as "this backs up
/// nothing".
fn describe_source(source: &kopiur_api::snapshot_policy::Source) -> String {
    if let Some(pvc) = &source.pvc {
        return format!("pvc/{}", pvc.name);
    }
    if let Some(selector) = &source.pvc_selector {
        let labels = selector
            .label_selector
            .as_ref()
            .map(kopiur_api::expand::label_selector_string)
            .unwrap_or_default();
        return format!("pvcSelector({labels})");
    }
    if let Some(nfs) = &source.nfs {
        return format!("nfs://{}{}", nfs.server, nfs.path);
    }
    // `Source` is a struct of optional shapes rather than an enum, so a source
    // that sets none of them is representable — and is exactly what a newer
    // operator's third source kind looks like to this build.
    "(unrecognized source)".to_string()
}

/// **Pure.** Everything the policy detail screen shows.
pub fn view_detail(
    policy: &SnapshotPolicy,
    schedules: &[Arc<SnapshotSchedule>],
    snapshots: &[Arc<Snapshot>],
) -> PolicyDetail {
    let name = policy.metadata.name.clone().unwrap_or_default();
    let conditions = policy
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or_default();

    let mut recent: Vec<Arc<Snapshot>> = snapshots
        .iter()
        .filter(|s| policy_of(s) == Some(name.as_str()))
        .cloned()
        .collect();
    recent.sort_by(|a, b| {
        sort_key(b)
            .cmp(&sort_key(a))
            .then_with(|| a.metadata.name.cmp(&b.metadata.name))
    });

    PolicyDetail {
        row: view_row_of(policy),
        identity: policy
            .status
            .as_ref()
            .and_then(|s| s.resolved.as_ref())
            .and_then(|r| r.identity.as_ref())
            .map(kopiur_api::identity::identity_string),
        sources: sources_view(policy),
        retention: policy.spec.retention.as_ref().map(retention_view),
        verification: verification_view(policy),
        schedules: schedules
            .iter()
            .filter(|s| fires_policy(s, policy))
            .map(|s| schedule_row(s))
            .collect(),
        recent_snapshots: recent
            .iter()
            .take(RECENT_SNAPSHOTS)
            .map(|s| view_row(s))
            .collect::<Vec<SnapshotRow>>(),
        gates: gate_hits(conditions, GateScope::covers_snapshot_policy),
        conditions: conditions_view(conditions),
    }
}

/// `GET /api/v1/policies?namespace=`
async fn list(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiQuery(q): UiQuery<NamespaceQuery>,
) -> Result<Json<Vec<PolicyRow>>, ApiError> {
    let client = client_for(&app, &id)?;
    let policies = app
        .source
        .list::<SnapshotPolicy>(&id, &client, q.namespace.as_deref())
        .await?;
    let mut rows: Vec<PolicyRow> = policies.iter().map(|p| view_row_of(p)).collect();
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(Json(rows))
}

/// `GET /api/v1/policies/{namespace}/{name}`
async fn detail(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiPath((namespace, name)): UiPath<(String, String)>,
) -> Result<Json<PolicyDetail>, ApiError> {
    let client = client_for(&app, &id)?;
    let policy = app
        .source
        .get::<SnapshotPolicy>(&id, &client, Some(&namespace), &name)
        .await?
        .ok_or_else(|| not_found(&namespace, &name))?;
    let schedules = app
        .source
        .list::<SnapshotSchedule>(&id, &client, Some(&namespace))
        .await?;
    let snapshots = app
        .source
        .list::<Snapshot>(&id, &client, Some(&namespace))
        .await?;
    Ok(Json(view_detail(&policy, &schedules, &snapshots)))
}

/// The 404 for a policy that is not there.
fn not_found(namespace: &str, name: &str) -> ApiError {
    problem(
        404,
        "not-found",
        format!("There is no SnapshotPolicy called {name} in namespace {namespace}."),
        "It was deleted or renamed — the SPA may be showing a link from a listing taken before \
         the change.",
        "reload the policies list to see what the cluster holds now",
    )
}

/// Whether a policy's repository set contains `key`. Exposed for the repository
/// detail screen's own join.
pub fn writes_into(policy: &SnapshotPolicy, key: &str) -> bool {
    let owner_ns = policy.metadata.namespace.clone().unwrap_or_default();
    repository_refs(&policy.spec).any(|r| repo_key(r, &owner_ns) == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::testutil::from_yaml;

    fn multi_repo() -> SnapshotPolicy {
        from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: media, labels: { tier: gold } }
spec:
  repositories:
    - { kind: Repository, name: nas }
    - { kind: ClusterRepository, name: shared }
  sources: [{ pvc: { name: data } }]
  retention: { keepDaily: 7, keepMonthly: 6 }
status:
  lastSuccessfulSnapshot: "2026-09-08T02:04:00Z"
  lastVerified: "2026-09-07T04:00:00Z"
  retention: { activeSnapshotCount: 42 }
  resolved:
    identity: { username: kopiur, hostname: media, sourcePath: /data }
    sources: [{ pvc: "media/data", sourcePath: "/data" }]
  verification:
    - repository: { kind: Repository, name: nas, namespace: media }
      lastVerified: "2026-09-07T04:00:00Z"
    - repository: { kind: ClusterRepository, name: shared }
"#,
        )
    }

    #[test]
    fn a_policy_row_names_every_repository_it_writes_into() {
        let row = view_row_of(&multi_repo());
        assert_eq!(row.namespace, "media");
        assert_eq!(row.name, "nightly");
        assert_eq!(
            row.repositories,
            vec!["Repository/media/nas", "ClusterRepository/shared"]
        );
        assert!(row.multi_repo);
        assert!(!row.suspended);
        assert_eq!(
            row.last_successful_snapshot.as_deref(),
            Some("2026-09-08T02:04:00Z")
        );
        assert_eq!(row.active_snapshot_count, Some(42));
    }

    #[test]
    fn a_single_repository_policy_is_not_multi_repo() {
        let single: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: simple, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
"#,
        );
        let row = view_row_of(&single);
        assert!(!row.multi_repo);
        assert_eq!(row.repositories, vec!["Repository/media/nas"]);
    }

    #[test]
    fn verification_is_per_repository_for_a_multi_repo_policy() {
        let views = verification_view(&multi_repo());
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].repository, "Repository/media/nas");
        assert_eq!(
            views[0].last_verified.as_deref(),
            Some("2026-09-07T04:00:00Z")
        );
        assert_eq!(views[1].repository, "ClusterRepository/shared");
        assert_eq!(
            views[1].last_verified, None,
            "a member that has never verified says so rather than borrowing a sibling's date"
        );
    }

    #[test]
    fn verification_falls_back_to_the_flat_timestamp_for_a_single_repo_policy() {
        let single: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: simple, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
status: { lastVerified: "2026-09-07T04:00:00Z" }
"#,
        );
        let views = verification_view(&single);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].repository, "Repository/media/nas");
        assert_eq!(
            views[0].last_verified.as_deref(),
            Some("2026-09-07T04:00:00Z")
        );
    }

    #[test]
    fn sources_prefer_what_the_last_run_expanded_to() {
        assert_eq!(sources_view(&multi_repo()), vec!["/data"]);
    }

    #[test]
    fn an_unexpanded_selector_is_shown_as_the_selector_not_as_nothing() {
        let selector: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: everything, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources:
    - pvcSelector: { labelSelector: { matchLabels: { backup: "yes" } } }
    - nfs: { server: "10.0.0.1", path: "/exports/media" }
"#,
        );
        assert_eq!(
            sources_view(&selector),
            vec!["pvcSelector(backup=yes)", "nfs://10.0.0.1/exports/media"]
        );
    }

    #[test]
    fn the_detail_joins_schedules_and_the_ten_most_recent_runs() {
        let policy = multi_repo();
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
        let unrelated: SnapshotSchedule = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotSchedule
metadata: { name: other-cron, namespace: media }
spec:
  policyRef: { name: weekly }
  schedule: { cron: "0 4 * * 0" }
"#,
        );
        let schedules = vec![Arc::new(by_ref), Arc::new(by_selector), Arc::new(unrelated)];

        let snapshots: Vec<Arc<Snapshot>> = (0..12)
            .map(|i| {
                Arc::new(from_yaml::<Snapshot>(&format!(
                    r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: nightly-{i:02}
  namespace: media
  labels: {{ "kopiur.home-operations.com/config": nightly }}
spec: {{ policyRef: {{ name: nightly }} }}
status:
  phase: Succeeded
  timing: {{ startTime: "2026-09-{:02}T02:00:00Z" }}
"#,
                    i + 1
                )))
            })
            .collect();

        let detail = view_detail(&policy, &schedules, &snapshots);
        let schedule_names: Vec<&str> = detail.schedules.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            schedule_names,
            vec!["nightly-cron", "gold-cron"],
            "both a direct reference and a matching selector fire this recipe"
        );
        assert_eq!(detail.recent_snapshots.len(), 10, "capped at ten");
        assert_eq!(
            detail.recent_snapshots[0].name, "nightly-11",
            "newest first"
        );
        assert_eq!(detail.identity.as_deref(), Some("kopiur@media:/data"));
        let retention = detail.retention.expect("this recipe configures retention");
        assert_eq!(retention.keep_daily, Some(7));
        assert_eq!(retention.keep_monthly, Some(6));
        assert_eq!(retention.keep_hourly, None);
    }

    #[test]
    fn writes_into_compares_normalized_repository_keys() {
        let policy = multi_repo();
        assert!(writes_into(&policy, "Repository/media/nas"));
        assert!(writes_into(&policy, "ClusterRepository/shared"));
        assert!(
            !writes_into(&policy, "Repository/apps/nas"),
            "a same-named repository in another namespace is a different repository"
        );
    }
}
