//! `GET /api/v1/status` — the overview the landing page renders: fleet counts,
//! work in flight, and what is stalled.
//!
//! # The report is the CLI's, verbatim
//!
//! The body is [`kopiur_ops::status::build_report`] serialized straight through,
//! not a second projection of it. `kubectl kopiur status` and the dashboard must
//! never disagree about whether a cluster is healthy, and the only way to
//! guarantee that is for both to render the same value — so
//! [`kopiur_ui_model::views::StatusOverview::report`] is deliberately opaque on
//! the wire and the SPA narrows it at the point of use.

use axum::extract::State;
use axum::{Json, Router, routing::get};
use chrono::Utc;

use kopiur_api::{
    ClusterRepository, Repository, Restore, Snapshot, SnapshotPolicy, SnapshotReplication,
    SnapshotSchedule,
};
use kopiur_ops::status::{StatusInputs, build_report};
use kopiur_ui_model::views::StatusOverview;

use crate::AppState;
use crate::api::problem::ApiError;
use crate::api::{NamespaceQuery, UiQuery, client_for};
use crate::auth::CurrentIdentity;
use crate::auth::identity::Identity;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new().route("/status", get(handler))
}

/// Copy the shared objects out of the read source into the owned `Vec`s
/// [`StatusInputs`] takes.
///
/// The clone is the price of `kopiur_ops` being front-end agnostic: its inputs
/// predate the UI's `Arc`-shaped cache and taking `&[Arc<K>]` there would make
/// the CLI carry a reference-counting scheme it has no use for.
fn owned<K: Clone>(items: Vec<std::sync::Arc<K>>) -> Vec<K> {
    items.iter().map(|k| K::clone(k)).collect()
}

/// Gather the seven kinds one status pass reads, under the caller's identity.
async fn load(
    app: &AppState,
    id: &Identity,
    namespace: Option<&str>,
) -> Result<StatusOverview, ApiError> {
    let client = client_for(app, id)?;

    let repositories = app
        .source
        .list::<Repository>(id, &client, namespace)
        .await?;
    let cluster_repositories = app
        .source
        .list::<ClusterRepository>(id, &client, None)
        .await?;
    let policies = app
        .source
        .list::<SnapshotPolicy>(id, &client, namespace)
        .await?;
    let schedules = app
        .source
        .list::<SnapshotSchedule>(id, &client, namespace)
        .await?;
    let snapshots = app.source.list::<Snapshot>(id, &client, namespace).await?;
    let restores = app.source.list::<Restore>(id, &client, namespace).await?;
    // `Some(..)` unconditionally: `Source::list` already answers an absent
    // `SnapshotReplication` CRD with an empty list (the impersonated arm maps
    // the type-level 404, the cache arm never sees one), so "the CRD is not
    // installed" and "there are none" have already been folded together by the
    // time the report is built. Threading `None` through would need a second,
    // out-of-band probe of the API surface to re-learn what the read source
    // deliberately smoothed over.
    let snapshot_replications = app
        .source
        .list::<SnapshotReplication>(id, &client, namespace)
        .await?;

    let inputs = StatusInputs {
        repositories: owned(repositories),
        cluster_repositories: owned(cluster_repositories),
        policies: owned(policies),
        schedules: owned(schedules),
        snapshot_replications: Some(owned(snapshot_replications)),
        snapshots: owned(snapshots),
        restores: owned(restores),
    };

    let report = serde_json::to_value(build_report(&inputs, None)).map_err(|e| {
        crate::api::problem::problem(
            500,
            "report-unserializable",
            "kopiur-ui assembled the cluster status but could not turn it into JSON.",
            format!("Serializing the status report failed: {e}. This is a bug in kopiur-ui, not a problem with the cluster."),
            "report this at https://github.com/home-operations/kopiur/issues",
        )
    })?;

    Ok(StatusOverview {
        report,
        now: Utc::now().to_rfc3339(),
    })
}

/// `GET /api/v1/status`
async fn handler(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiQuery(q): UiQuery<NamespaceQuery>,
) -> Result<Json<StatusOverview>, ApiError> {
    Ok(Json(load(&app, &id, q.namespace.as_deref()).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::testutil::from_yaml;

    #[test]
    fn the_report_serializes_to_the_shape_the_cli_prints() {
        let repo: Repository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: { name: nas, namespace: media }
spec:
  backend: { filesystem: { path: /repo } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
status:
  phase: Ready
  backend: Filesystem
"#,
        );
        let inputs = StatusInputs {
            repositories: vec![repo],
            cluster_repositories: Vec::new(),
            policies: Vec::new(),
            schedules: Vec::new(),
            snapshot_replications: Some(Vec::new()),
            snapshots: Vec::new(),
            restores: Vec::new(),
        };
        let value = serde_json::to_value(build_report(&inputs, None)).unwrap();
        assert_eq!(
            value["repositories"][0]["name"], "nas",
            "the wire body is the ops report itself: {value}"
        );
        assert_eq!(value["repositories"][0]["phase"], "Ready");
        assert!(
            value.get("inFlight").is_some(),
            "the report's own camelCase keys travel untouched: {value}"
        );
    }

    #[test]
    fn owned_copies_out_of_the_shared_cache() {
        let repo: Repository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: { name: nas, namespace: media }
spec:
  backend: { filesystem: { path: /repo } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
"#,
        );
        let shared = vec![std::sync::Arc::new(repo)];
        let copied = owned(shared.clone());
        assert_eq!(copied.len(), 1);
        assert_eq!(copied[0].metadata.name.as_deref(), Some("nas"));
        assert_eq!(
            std::sync::Arc::strong_count(&shared[0]),
            1,
            "copying out must not leave the cache's object borrowed"
        );
    }
}
