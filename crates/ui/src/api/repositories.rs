//! The repository endpoints: the fleet list and per-repository detail, covering
//! both `Repository` and `ClusterRepository`.
//!
//! # One row shape for two kinds
//!
//! `Repository` and `ClusterRepository` have separate CRDs — one namespaced, one
//! not — but the same story to tell, so both project onto
//! [`RepositorySummary`]. The kind survives as a field rather than as two
//! endpoints, because a fleet view that made the user look in two places for
//! "my repositories" would be answering the CRD's question instead of theirs.

use std::sync::Arc;

use axum::extract::State;
use axum::{Json, Router, routing::get};
use k8s_openapi::api::batch::v1::Job;

use kopiur_api::cluster_repository::AllowedNamespaces;
use kopiur_api::common::{RepositoryKind, RepositoryMode, repo_key};
use kopiur_api::gates::GateScope;
use kopiur_api::{
    ClusterRepository, Maintenance, Repository, RepositoryPhase, RepositoryReplication,
    SnapshotPolicy, SnapshotReplication,
};
use kopiur_ui_model::graph::GateHit;
use kopiur_ui_model::views::{
    CatalogView, HealthProbeView, PolicyRef, RepositoryDetail, RepositorySummary, SeedView,
    ServerView, SessionInfo,
};

use crate::AppState;
use crate::api::graph::repository_health;
use crate::api::maintenance::maintenance_row;
use crate::api::policies::writes_into;
use crate::api::problem::{ApiError, problem};
use crate::api::{
    NamespaceQuery, RepositoryKindPath, UiPath, UiQuery, client_for, conditions_view,
    covering_maintenances, gate_hits, ops_ctx, repo_phase_view,
};
use crate::auth::CurrentIdentity;
use crate::config::WireLimits;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/repositories", get(list))
        .route("/repositories/{kind}/{name}", get(detail))
}

/// The fields of a repository the two CRDs share, gathered so one projection can
/// serve both without a trait.
struct RepoFacts<'a> {
    kind: RepositoryKind,
    name: String,
    namespace: Option<String>,
    phase: Option<&'a RepositoryPhase>,
    backend: Option<String>,
    mode: RepositoryMode,
    server_configured: bool,
    suspended: bool,
    snapshot_count: Option<i64>,
    total_size_bytes: Option<i64>,
    index_blob_count: Option<i64>,
    last_observed_at: Option<String>,
    server_endpoint: Option<String>,
    allowed_namespace_count: Option<i64>,
}

/// **Pure.** `spec.mode` as the string every other kopiur front end prints.
///
/// An exhaustive `match` rather than `format!("{mode:?}")`, producing the same
/// two strings: `kopiur_ops::status` builds the CLI's `MODE` column with the
/// `Debug` spelling (`crates/ops/src/status.rs`), `/api/v1/status` ships that
/// report verbatim, and this row has to agree with both — but a `Debug` call
/// would let a third variant ship a new string with no compile error, and the
/// agreement is the whole point. `mode_is_the_string_status_and_the_cli_print`
/// pins the two against each other.
///
/// A second spelling here would mean one API answering `mode` two ways, which is
/// the collision this replaced.
///
/// How clients *reach* the repository is `server_backed`, a separate field:
/// access path and access rights are independent, and a read-only repository
/// can be either.
fn mode_label(mode: RepositoryMode) -> String {
    match mode {
        RepositoryMode::ReadWrite => "ReadWrite",
        RepositoryMode::ReadOnly => "ReadOnly",
    }
    .to_string()
}

/// **Pure.** The shared projection.
///
/// `kind` and `kind_path` are both derived from the one [`RepositoryKind`] on
/// [`RepoFacts`], never written out side by side: the display kind and the URL
/// segment differ in case and punctuation, and a row whose link disagreed with
/// its own label is exactly the mismatch `RepositoryKindPath` exists to end.
fn summary_from(facts: &RepoFacts<'_>, gates: &[GateHit]) -> RepositorySummary {
    RepositorySummary {
        kind: facts.kind.kind_str().to_string(),
        kind_path: RepositoryKindPath::from_kind(facts.kind)
            .as_path()
            .to_string(),
        name: facts.name.clone(),
        namespace: facts.namespace.clone(),
        phase: facts.phase.map(repo_phase_view),
        health: repository_health(facts.phase, facts.suspended, gates),
        backend: facts.backend.clone(),
        mode: mode_label(facts.mode),
        server_backed: facts.server_configured,
        suspended: facts.suspended,
        snapshot_count: facts.snapshot_count,
        total_size_bytes: facts.total_size_bytes,
        index_blob_count: facts.index_blob_count,
        last_observed_at: facts.last_observed_at.clone(),
        server_endpoint: facts.server_endpoint.clone(),
        allowed_namespace_count: facts.allowed_namespace_count,
    }
}

/// **Pure.** One namespaced `Repository` as a table row.
pub fn view_repository(repo: &Repository) -> RepositorySummary {
    let status = repo.status.as_ref();
    let conditions = status.map(|s| s.conditions.as_slice()).unwrap_or_default();
    let stats = status.and_then(|s| s.storage_stats.as_ref());
    let facts = RepoFacts {
        kind: RepositoryKind::Repository,
        name: repo.metadata.name.clone().unwrap_or_default(),
        namespace: repo.metadata.namespace.clone(),
        phase: status.and_then(|s| s.phase.as_ref()),
        backend: status
            .and_then(|s| s.backend.clone())
            .or_else(|| Some(repo.spec.backend.kind_str().to_string())),
        mode: repo.spec.mode,
        server_configured: repo.spec.server.is_some(),
        suspended: repo.spec.suspend,
        snapshot_count: stats.and_then(|s| s.snapshot_count),
        total_size_bytes: stats.and_then(|s| s.total_size_bytes),
        index_blob_count: stats.and_then(|s| s.index_blob_count),
        last_observed_at: stats.and_then(|s| s.last_observed_at.clone()),
        server_endpoint: status
            .and_then(|s| s.server.as_ref())
            .and_then(|s| s.endpoint.clone()),
        allowed_namespace_count: None,
    };
    summary_from(&facts, &gate_hits(conditions, GateScope::covers_repository))
}

/// **Pure.** One `ClusterRepository` as a table row.
pub fn view_cluster_repository(repo: &ClusterRepository) -> RepositorySummary {
    let status = repo.status.as_ref();
    let conditions = status.map(|s| s.conditions.as_slice()).unwrap_or_default();
    let stats = status.and_then(|s| s.storage_stats.as_ref());
    let facts = RepoFacts {
        kind: RepositoryKind::ClusterRepository,
        name: repo.metadata.name.clone().unwrap_or_default(),
        namespace: None,
        phase: status.and_then(|s| s.phase.as_ref()),
        backend: status
            .and_then(|s| s.backend.clone())
            .or_else(|| Some(repo.spec.backend.kind_str().to_string())),
        mode: repo.spec.mode,
        server_configured: repo.spec.server.is_some(),
        suspended: repo.spec.suspend,
        snapshot_count: stats.and_then(|s| s.snapshot_count),
        total_size_bytes: stats.and_then(|s| s.total_size_bytes),
        index_blob_count: stats.and_then(|s| s.index_blob_count),
        last_observed_at: stats.and_then(|s| s.last_observed_at.clone()),
        server_endpoint: status
            .and_then(|s| s.server.as_ref())
            .and_then(|s| s.endpoint.clone()),
        allowed_namespace_count: status
            .and_then(|s| s.allowed_namespace_count)
            .or_else(|| declared_namespace_count(&repo.spec.allowed_namespaces)),
    };
    summary_from(&facts, &gate_hits(conditions, GateScope::covers_repository))
}

/// **Pure.** How many namespaces the *spec* names, for a repository the
/// controller has not counted yet. Exhaustive: only a `List` can be counted
/// without asking the apiserver, and a `Selector`/`All` honestly has no
/// spec-side answer.
fn declared_namespace_count(allowed: &AllowedNamespaces) -> Option<i64> {
    match allowed {
        AllowedNamespaces::List(names) => Some(names.len() as i64),
        AllowedNamespaces::Selector(_) | AllowedNamespaces::All(_) => None,
    }
}

/// **Pure.** Everything the detail screen shows, given the objects around this
/// repository.
///
/// `key` is the repository's [`repo_key`] — the one string every "does this
/// point at me?" comparison here joins on.
#[allow(clippy::too_many_arguments)]
pub fn view_detail(
    summary: RepositorySummary,
    key: &str,
    kind: RepositoryKind,
    identity_cluster: Option<String>,
    catalog: Option<CatalogView>,
    health: Option<HealthProbeView>,
    server: Option<ServerView>,
    seed: Option<SeedView>,
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
    policies: &[Arc<SnapshotPolicy>],
    snapshot_replications: &[Arc<SnapshotReplication>],
    repository_replications: &[Arc<RepositoryReplication>],
    maintenances: &[Arc<Maintenance>],
    sessions: Vec<SessionInfo>,
) -> RepositoryDetail {
    let name = summary.name.clone();
    RepositoryDetail {
        identity_cluster,
        catalog,
        health,
        server,
        seed,
        maintenance: covering_maintenance(maintenances, kind, &name, summary.namespace.as_deref())
            .map(|m| maintenance_row(&m)),
        gates: gate_hits(conditions, GateScope::covers_repository),
        conditions: conditions_view(conditions),
        policies: policies_writing_into(policies, key),
        replications_out: replications_from(snapshot_replications, repository_replications, key),
        replications_in: replications_into(snapshot_replications, key),
        sessions,
        summary,
    }
}

/// **Pure.** The policies whose repository set contains `key`.
///
/// The predicate itself is [`writes_into`], next to the policy views: it is the
/// same question that screen asks, and it was inlined here as a second copy.
fn policies_writing_into(policies: &[Arc<SnapshotPolicy>], key: &str) -> Vec<PolicyRef> {
    policies
        .iter()
        .filter(|p| writes_into(p, key))
        .map(|p| PolicyRef {
            namespace: p.metadata.namespace.clone().unwrap_or_default(),
            name: p.metadata.name.clone().unwrap_or_default(),
        })
        .collect()
}

/// **Pure.** Names of the replications that *read* from `key` — both kinds, since
/// a repository is equally drained by a snapshot copy and by a blob sync.
fn replications_from(
    snapshot: &[Arc<SnapshotReplication>],
    repository: &[Arc<RepositoryReplication>],
    key: &str,
) -> Vec<String> {
    let from_snapshot = snapshot.iter().filter_map(|r| {
        let owner_ns = r.metadata.namespace.clone().unwrap_or_default();
        (repo_key(&r.spec.source_ref, &owner_ns) == key)
            .then(|| r.metadata.name.clone().unwrap_or_default())
    });
    let from_repository = repository.iter().filter_map(|r| {
        let owner_ns = r.metadata.namespace.clone().unwrap_or_default();
        (repo_key(&r.spec.source_ref, &owner_ns) == key)
            .then(|| r.metadata.name.clone().unwrap_or_default())
    });
    from_snapshot.chain(from_repository).collect()
}

/// **Pure.** Names of the replications that *write into* `key`.
///
/// Only `SnapshotReplication` can: a `RepositoryReplication` writes to a bare
/// backend, which is not a repository CR and therefore not something that can
/// name this one as a destination.
fn replications_into(snapshot: &[Arc<SnapshotReplication>], key: &str) -> Vec<String> {
    snapshot
        .iter()
        .filter_map(|r| {
            let owner_ns = r.metadata.namespace.clone().unwrap_or_default();
            (repo_key(&r.spec.destination_ref, &owner_ns) == key)
                .then(|| r.metadata.name.clone().unwrap_or_default())
        })
        .collect()
}

/// **Pure.** The `Maintenance` governing this repository, managed or foreign.
///
/// The namespace guard lives in [`covering_maintenances`], shared with the fleet
/// graph — which did not have it, and coloured a repository degraded because a
/// same-named one in another namespace had a failing compaction.
fn covering_maintenance(
    maintenances: &[Arc<Maintenance>],
    kind: RepositoryKind,
    name: &str,
    namespace: Option<&str>,
) -> Option<Arc<Maintenance>> {
    covering_maintenances(maintenances, kind, name, namespace)
        .next()
        .cloned()
}

/// **Pure.** When a browse session's Job will be reaped, from its start time and
/// the deadline the session was launched with.
fn session_expiry(job: &Job) -> Option<String> {
    let started = job
        .status
        .as_ref()
        .and_then(|s| s.start_time.as_ref())
        .and_then(kopiur_ops::snapshots::meta_time)?;
    let deadline = job.spec.as_ref().and_then(|s| s.active_deadline_seconds)?;
    Some((started + chrono::Duration::seconds(deadline)).to_rfc3339())
}

/// **Pure.** A found session Job as the SPA's session record.
///
/// `pod` is left unset: naming it needs a `pods` LIST the read API does not
/// perform, and the browse endpoints (which do exec into the pod) resolve it
/// themselves. `reused` is always true — a session the detail screen *finds* is
/// by definition one that already existed.
fn session_info(job: &Job, limits: WireLimits) -> SessionInfo {
    SessionInfo {
        namespace: job.metadata.namespace.clone().unwrap_or_default(),
        job: job.metadata.name.clone().unwrap_or_default(),
        pod: None,
        reused: true,
        expires_at: session_expiry(job),
        download_max_bytes: limits.download_max_bytes,
        manifest_max_bytes: limits.manifest_max_bytes,
    }
}

/// `GET /api/v1/repositories?namespace=`
async fn list(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiQuery(q): UiQuery<NamespaceQuery>,
) -> Result<Json<Vec<RepositorySummary>>, ApiError> {
    let namespace = q.namespace.as_deref();
    let client = client_for(&app, &id)?;
    let repositories = app
        .source
        .list::<Repository>(&id, &client, namespace)
        .await?;
    let cluster = app
        .source
        .list::<ClusterRepository>(&id, &client, None)
        .await?;

    let mut rows: Vec<RepositorySummary> = repositories
        .iter()
        .map(|r| view_repository(r))
        .chain(cluster.iter().map(|r| view_cluster_repository(r)))
        .collect();
    // Stable, kind-then-namespace-then-name: the table must not reshuffle
    // between polls just because a watch delivered objects in a new order.
    rows.sort_by(|a, b| (&a.kind, &a.namespace, &a.name).cmp(&(&b.kind, &b.namespace, &b.name)));
    Ok(Json(rows))
}

/// `GET /api/v1/repositories/{kind}/{name}?namespace=`
async fn detail(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiPath((kind, name)): UiPath<(RepositoryKindPath, String)>,
    UiQuery(q): UiQuery<NamespaceQuery>,
) -> Result<Json<RepositoryDetail>, ApiError> {
    let client = client_for(&app, &id)?;
    let namespace = q.namespace.as_deref();

    let (summary, key, identity_cluster, catalog, health, server, seed, conditions, repo_ns) =
        match kind {
            RepositoryKindPath::Repository => {
                let ns = namespace.ok_or_else(missing_namespace)?;
                let repo = app
                    .source
                    .get::<Repository>(&id, &client, Some(ns), &name)
                    .await?
                    .ok_or_else(|| not_found("Repository", Some(ns), &name))?;
                let status = repo.status.clone();
                (
                    view_repository(&repo),
                    format!("Repository/{ns}/{name}"),
                    repo.spec
                        .identity_defaults
                        .as_ref()
                        .and_then(|d| d.cluster.clone()),
                    status
                        .as_ref()
                        .and_then(|s| s.catalog.as_ref())
                        .map(catalog_view),
                    status
                        .as_ref()
                        .and_then(|s| s.health.as_ref())
                        .map(health_view),
                    status
                        .as_ref()
                        .and_then(|s| s.server.as_ref())
                        .map(server_view),
                    status.as_ref().and_then(|s| s.seed.as_ref()).map(seed_view),
                    status.map(|s| s.conditions).unwrap_or_default(),
                    Some(ns.to_string()),
                )
            }
            RepositoryKindPath::ClusterRepository => {
                let repo = app
                    .source
                    .get::<ClusterRepository>(&id, &client, None, &name)
                    .await?
                    .ok_or_else(|| not_found("ClusterRepository", None, &name))?;
                let status = repo.status.clone();
                (
                    view_cluster_repository(&repo),
                    format!("ClusterRepository/{name}"),
                    repo.spec
                        .identity_defaults
                        .as_ref()
                        .and_then(|d| d.cluster.clone()),
                    status
                        .as_ref()
                        .and_then(|s| s.catalog.as_ref())
                        .map(catalog_view),
                    status
                        .as_ref()
                        .and_then(|s| s.health.as_ref())
                        .map(health_view),
                    status
                        .as_ref()
                        .and_then(|s| s.server.as_ref())
                        .map(server_view),
                    status.as_ref().and_then(|s| s.seed.as_ref()).map(seed_view),
                    status.map(|s| s.conditions).unwrap_or_default(),
                    None,
                )
            }
        };

    // The referents are read cluster-wide: a policy in another namespace can
    // legitimately write into a `ClusterRepository`, and a caller who may not
    // see it simply gets a shorter list rather than an error.
    let policies = app
        .source
        .list::<SnapshotPolicy>(&id, &client, None)
        .await?;
    let snapshot_replications = app
        .source
        .list::<SnapshotReplication>(&id, &client, None)
        .await?;
    let repository_replications = app
        .source
        .list::<RepositoryReplication>(&id, &client, None)
        .await?;
    let maintenances = app.source.list::<Maintenance>(&id, &client, None).await?;

    let sessions = load_sessions(&app, client, kind.kind(), repo_ns.as_deref(), &name).await?;

    Ok(Json(view_detail(
        summary,
        &key,
        kind.kind(),
        identity_cluster,
        catalog,
        health,
        server,
        seed,
        &conditions,
        &policies,
        &snapshot_replications,
        &repository_replications,
        &maintenances,
        sessions,
    )))
}

/// Look for a warm browse session against this repository, under the caller's
/// own client — a session Job is an ordinary `Job` the caller either may read or
/// may not.
///
/// A `ClusterRepository`'s session lives in the operator's namespace; a
/// namespaced repository's in its own. With no operator namespace configured
/// there is nowhere to look, and reporting "no sessions" is the honest answer.
async fn load_sessions(
    app: &AppState,
    client: kube::Client,
    kind: RepositoryKind,
    repo_namespace: Option<&str>,
    name: &str,
) -> Result<Vec<SessionInfo>, ApiError> {
    let job_namespace = match kind {
        RepositoryKind::Repository => repo_namespace.map(str::to_string),
        RepositoryKind::ClusterRepository => app.cfg.operator_namespace.clone(),
    };
    let Some(job_namespace) = job_namespace else {
        return Ok(Vec::new());
    };
    let ctx = ops_ctx(&app.cfg, client, Some(&job_namespace));
    let found = kopiur_ops::browse::session::find_session_job(
        &ctx,
        &job_namespace,
        kind,
        repo_namespace,
        name,
    )
    .await?;
    let limits = WireLimits::from_config(&app.cfg);
    Ok(found.iter().map(|job| session_info(job, limits)).collect())
}

/// **Pure.** `Repository.status.catalog` as the wire view.
fn catalog_view(c: &kopiur_api::repository::CatalogStatus) -> CatalogView {
    CatalogView {
        discovered_backup_count: c.discovered_backup_count,
        foreign_snapshot_count: c.foreign_snapshot_count,
        last_refresh_at: c.last_refresh_at.clone(),
    }
}

/// **Pure.** `Repository.status.health` as the wire view.
fn health_view(h: &kopiur_api::repository::RepositoryHealthStatus) -> HealthProbeView {
    HealthProbeView {
        last_probe_at: h.last_probe_at.clone(),
        last_healthy_at: h.last_healthy_at.clone(),
        consecutive_probe_failures: h.consecutive_probe_failures,
    }
}

/// **Pure.** `Repository.status.server` as the wire view. The generated Secret
/// reference is deliberately dropped: the UI never names a credential.
fn server_view(s: &kopiur_api::server::ServerStatus) -> ServerView {
    ServerView {
        endpoint: s.endpoint.clone(),
        read_only: s.read_only,
        auth_mode: s.auth_mode.clone(),
    }
}

/// **Pure.** `Repository.status.seed` as the wire view.
fn seed_view(s: &kopiur_api::seed::SeedStatus) -> SeedView {
    SeedView {
        mode: s.mode.as_ref().map(|m| format!("{m:?}")),
        source: s.source.clone(),
        seeded_at: s.seeded_at.clone(),
        snapshots_copied: s.snapshot_count,
    }
}

/// The 400 a namespaced repository lookup answers when the caller named no
/// namespace.
fn missing_namespace() -> ApiError {
    problem(
        400,
        "namespace-required",
        "A Repository lookup needs the namespace it lives in.",
        "`Repository` is namespaced, so its name alone does not identify one object — two \
         namespaces may each hold a repository called the same thing.",
        "add ?namespace=<namespace> to the request, or ask for a cluster-repository instead",
    )
}

/// The 404 for a repository that is not there.
fn not_found(kind: &str, namespace: Option<&str>, name: &str) -> ApiError {
    let scope = kopiur_ops::scope_suffix(namespace);
    problem(
        404,
        "not-found",
        format!("There is no {kind} called {name}{scope}."),
        "It was deleted, renamed, or never existed — the SPA may be showing a link from a \
         listing taken before the change.",
        "reload the repositories list to see what the cluster holds now",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::testutil::from_yaml;
    use kopiur_ui_model::graph::Health;
    use kopiur_ui_model::views::RepositoryPhaseView;

    /// Deliberately not the defaults: a session must publish what this
    /// deployment is configured with.
    fn test_limits() -> WireLimits {
        WireLimits {
            download_max_bytes: 7_000,
            manifest_max_bytes: 900,
        }
    }

    fn nas() -> Repository {
        from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: { name: nas, namespace: media }
spec:
  backend: { s3: { bucket: backups, prefix: nas/ } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
  identityDefaults: { cluster: east }
status:
  phase: Ready
  backend: S3
  storageStats:
    snapshotCount: 412
    totalSizeBytes: 987654321
    indexBlobCount: 17
    lastObservedAt: "2026-09-08T01:00:00Z"
"#,
        )
    }

    #[test]
    fn a_repository_row_carries_its_stats_and_health() {
        let row = view_repository(&nas());
        assert_eq!(row.kind, "Repository");
        assert_eq!(row.namespace.as_deref(), Some("media"));
        assert_eq!(row.phase, Some(RepositoryPhaseView::Ready));
        assert_eq!(row.health, Health::Healthy);
        assert_eq!(row.backend.as_deref(), Some("S3"));
        assert_eq!(row.mode, "ReadWrite", "spec.mode, defaulted");
        assert!(!row.server_backed, "no repository server is configured");
        assert!(!row.suspended);
        assert_eq!(row.snapshot_count, Some(412));
        assert_eq!(row.total_size_bytes, Some(987_654_321));
        assert_eq!(row.index_blob_count, Some(17));
        assert_eq!(
            row.allowed_namespace_count, None,
            "only a cluster repository has one"
        );
    }

    /// The row carries BOTH the CRD kind (for display) and the URL segment (for
    /// linking), and the segment is one the routes accept — so the SPA never
    /// builds `/repositories/{kind}/…` from `kind` and gets it wrong.
    #[test]
    fn a_row_carries_the_url_segment_its_own_detail_route_takes() {
        let namespaced = view_repository(&nas());
        assert_eq!(namespaced.kind, "Repository");
        assert_eq!(namespaced.kind_path, "repository");

        let cluster: ClusterRepository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: ClusterRepository
metadata: { name: shared }
spec:
  backend: { filesystem: { path: /repo } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
  allowedNamespaces: { all: true }
"#,
        );
        let row = view_cluster_repository(&cluster);
        assert_eq!(row.kind, "ClusterRepository");
        assert_eq!(
            row.kind_path, "cluster-repository",
            "the routing token is kebab; `kind` is the CRD spelling"
        );

        for row in [&namespaced, &row] {
            assert_eq!(
                RepositoryKindPath::parse(&row.kind_path).map(RepositoryKindPath::kind),
                Some(
                    RepositoryKindPath::parse(&row.kind)
                        .expect("the CRD kind parses too")
                        .kind()
                ),
                "the segment the row hands the SPA must resolve to the row's own kind"
            );
        }
    }

    #[test]
    fn a_suspended_repository_is_suspended_not_ready() {
        let suspended: Repository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: { name: paused, namespace: media }
spec:
  backend: { filesystem: { path: /repo } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
  suspend: true
status: { phase: Ready }
"#,
        );
        let row = view_repository(&suspended);
        assert_eq!(row.health, Health::Suspended);
        assert!(row.suspended);
        assert_eq!(
            row.phase,
            Some(RepositoryPhaseView::Ready),
            "the phase is reported as written; only the health reflects the pause"
        );
    }

    #[test]
    fn the_backend_falls_back_to_the_spec_before_the_controller_mirrors_it() {
        let fresh: Repository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: { name: new, namespace: media }
spec:
  backend: { filesystem: { path: /repo } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
"#,
        );
        let row = view_repository(&fresh);
        assert_eq!(
            row.backend.as_deref(),
            Some("Filesystem"),
            "an unreconciled repository still knows its own backend"
        );
        assert_eq!(row.health, Health::Unknown);
    }

    #[test]
    fn the_access_path_and_the_access_rights_are_separate_fields() {
        // A read-ONLY repository fronted by a server: the pairing that a single
        // `mode` field could not express, and the reason `server_backed` exists.
        let served: Repository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: { name: served, namespace: media }
spec:
  backend: { filesystem: { path: /repo } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
  mode: ReadOnly
  server: { readOnly: true }
status:
  phase: Ready
  server: { endpoint: "kopiur-served.media.svc:51515", readOnly: true, authMode: Generate }
"#,
        );
        let row = view_repository(&served);
        assert_eq!(row.mode, "ReadOnly");
        assert!(row.server_backed);
        assert_eq!(
            row.server_endpoint.as_deref(),
            Some("kopiur-served.media.svc:51515")
        );
    }

    #[test]
    fn mode_is_the_string_status_and_the_cli_print() {
        // `/api/v1/status` ships `kopiur_ops::status`'s report verbatim, and its
        // `mode` is `format!("{:?}", spec.mode)`. One API must not answer `mode`
        // two ways, so the two spellings are pinned against each other here
        // rather than trusted to stay aligned.
        for mode in [RepositoryMode::ReadWrite, RepositoryMode::ReadOnly] {
            assert_eq!(
                mode_label(mode),
                format!("{mode:?}"),
                "the row's MODE must read exactly as the CLI's column does"
            );
        }
    }

    #[test]
    fn a_cluster_repository_counts_its_namespaces_from_the_spec_until_the_controller_does() {
        let listed: ClusterRepository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: ClusterRepository
metadata: { name: shared }
spec:
  backend: { s3: { bucket: shared } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
  allowedNamespaces: { list: [prod, staging, dev] }
"#,
        );
        let row = view_cluster_repository(&listed);
        assert_eq!(row.kind, "ClusterRepository");
        assert_eq!(row.namespace, None);
        assert_eq!(row.allowed_namespace_count, Some(3));

        let selected: ClusterRepository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: ClusterRepository
metadata: { name: selected }
spec:
  backend: { s3: { bucket: shared } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
  allowedNamespaces: { selector: { matchLabels: { backup: "yes" } } }
"#,
        );
        assert_eq!(
            view_cluster_repository(&selected).allowed_namespace_count,
            None,
            "a selector cannot be counted without asking the apiserver"
        );

        let counted: ClusterRepository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: ClusterRepository
metadata: { name: counted }
spec:
  backend: { s3: { bucket: shared } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
  allowedNamespaces: { all: true }
status: { phase: Ready, allowedNamespaceCount: 12 }
"#,
        );
        assert_eq!(
            view_cluster_repository(&counted).allowed_namespace_count,
            Some(12),
            "the controller's count wins wherever it exists"
        );
    }

    #[test]
    fn the_detail_joins_policies_and_replications_by_repository_key() {
        let policy_here: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
"#,
        );
        // Same repository NAME, different namespace: must not match.
        let policy_elsewhere: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: other, namespace: apps }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
"#,
        );
        let out: SnapshotReplication = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotReplication
metadata: { name: offsite, namespace: media }
spec:
  sourceRef: { kind: Repository, name: nas }
  destinationRef: { kind: ClusterRepository, name: shared }
  schedule: { cron: "0 3 * * *" }
"#,
        );
        let into: SnapshotReplication = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotReplication
metadata: { name: inbound, namespace: media }
spec:
  sourceRef: { kind: ClusterRepository, name: shared }
  destinationRef: { kind: Repository, name: nas }
  schedule: { cron: "0 6 * * *" }
"#,
        );
        let blob: RepositoryReplication = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: RepositoryReplication
metadata: { name: blobsync, namespace: media }
spec:
  sourceRef: { kind: Repository, name: nas }
  destination: { s3: { bucket: dr } }
  schedule: { cron: "0 5 * * *" }
"#,
        );

        let policies = vec![Arc::new(policy_here), Arc::new(policy_elsewhere)];
        let snapshot_replications = vec![Arc::new(out), Arc::new(into)];
        let repository_replications = vec![Arc::new(blob)];

        let detail = view_detail(
            view_repository(&nas()),
            "Repository/media/nas",
            RepositoryKind::Repository,
            Some("east".into()),
            None,
            None,
            None,
            None,
            &[],
            &policies,
            &snapshot_replications,
            &repository_replications,
            &[],
            Vec::new(),
        );

        assert_eq!(
            detail.policies,
            vec![PolicyRef {
                namespace: "media".into(),
                name: "nightly".into()
            }],
            "a same-named repository in another namespace is a different repository"
        );
        assert_eq!(detail.replications_out, vec!["offsite", "blobsync"]);
        assert_eq!(detail.replications_in, vec!["inbound"]);
        assert_eq!(detail.identity_cluster.as_deref(), Some("east"));
        assert!(detail.maintenance.is_none());
    }

    #[test]
    fn maintenance_is_matched_within_the_repositorys_own_namespace() {
        let here: Maintenance = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Maintenance
metadata: { name: nas-maint, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  schedule: { quick: { cron: "0 * * * *" }, full: { cron: "0 4 * * 0" } }
  ownership: { owner: kopiur/media/nas }
"#,
        );
        // A Maintenance for a same-named repository in another namespace.
        let elsewhere: Maintenance = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Maintenance
metadata: { name: nas-maint, namespace: apps }
spec:
  repository: { kind: Repository, name: nas }
  schedule: { quick: { cron: "0 * * * *" }, full: { cron: "0 4 * * 0" } }
  ownership: { owner: kopiur/apps/nas }
"#,
        );
        let maintenances = vec![Arc::new(elsewhere), Arc::new(here)];
        let found = covering_maintenance(
            &maintenances,
            RepositoryKind::Repository,
            "nas",
            Some("media"),
        )
        .expect("the maintenance in the repository's own namespace covers it");
        assert_eq!(found.metadata.namespace.as_deref(), Some("media"));
    }

    #[test]
    fn a_session_job_becomes_a_reused_session_with_an_expiry() {
        let job: Job = serde_json::from_value(serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": { "name": "kopiur-browse-nas-deadbeef", "namespace": "media" },
            "spec": { "activeDeadlineSeconds": 1020 },
            "status": { "startTime": "2026-09-08T10:00:00Z" }
        }))
        .unwrap();
        let info = session_info(&job, test_limits());
        assert_eq!(info.namespace, "media");
        assert_eq!(info.job, "kopiur-browse-nas-deadbeef");
        assert!(
            info.reused,
            "a session the detail screen finds already existed"
        );
        assert_eq!(
            (info.download_max_bytes, info.manifest_max_bytes),
            (7_000, 900),
            "a session listed on the detail screen publishes the same caps the \
             browse endpoints do, so one screen cannot disagree with the other"
        );
        assert_eq!(info.pod, None, "naming the pod is the browse API's job");
        assert!(
            info.expires_at
                .as_deref()
                .is_some_and(|t| t.starts_with("2026-09-08T10:17:00")),
            "start + activeDeadlineSeconds, got {:?}",
            info.expires_at
        );
    }

    #[test]
    fn a_session_job_with_no_start_time_has_no_expiry_rather_than_a_guess() {
        let job: Job = serde_json::from_value(serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": { "name": "kopiur-browse-nas-deadbeef", "namespace": "media" },
            "spec": { "activeDeadlineSeconds": 1020 }
        }))
        .unwrap();
        assert_eq!(session_info(&job, test_limits()).expires_at, None);
    }

    #[test]
    fn the_not_found_problem_names_the_scope() {
        let err = not_found("Repository", Some("media"), "nas");
        assert_eq!(err.0.status, 404);
        assert!(err.0.what.contains("media"), "got {}", err.0.what);
        assert!(!err.0.fix.is_empty());
    }
}
