//! `GET /api/v1/graph` — the repository topology: repositories, the policies and
//! namespaces feeding them, and the replication edges between them.
//!
//! # Node ids are repository keys
//!
//! Every repository node's id is [`kopiur_api::common::repo_key`] of the object
//! it stands for, so an edge built from a `RepositoryRef` lands on the right
//! node without a lookup table — and a ref that resolves to no object is
//! detected by exactly the same string comparison. Those dangling refs are not
//! dropped: they become `missing: true` ghost nodes at [`Health::Failed`],
//! because a policy pointed at a repository that does not exist is the failure
//! most worth drawing.
//!
//! # Why the builder is pure
//!
//! [`build`] takes the objects and a clock and returns the whole graph. Every
//! interesting property — which nodes exist, which edges join them, what colour
//! each is — is therefore a table test over YAML fixtures rather than something
//! only a live cluster can show.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::{Json, Router, routing::get};
use chrono::{DateTime, Utc};

use kopiur_api::backend::Backend;
use kopiur_api::cluster_repository::AllowedNamespaces;
use kopiur_api::common::{RepositoryKind, RepositoryRef, repo_key};
use kopiur_api::expand::label_selector_string;
use kopiur_api::gates::GateScope;
use kopiur_api::seed::seed_repository_ref;
use kopiur_api::snapshot_policy::repository_refs;
use kopiur_api::{
    ClusterRepository, Maintenance, Repository, RepositoryPhase, RepositoryReplication,
    SnapshotPolicy, SnapshotReplication,
};
use kopiur_ui_model::graph::{
    EdgeKind, GateHit, GraphEdge, GraphNode, Health, NodeKind, RepositoryGraph,
};

use crate::AppState;
use crate::api::problem::ApiError;
use crate::api::{
    NamespaceQuery, client_for, gate_hits, repository_replication_phase_view,
    snapshot_replication_phase_view,
};
use crate::auth::CurrentIdentity;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new().route("/graph", get(handler))
}

/// Everything [`build`] draws from, gathered by [`load`].
///
/// Borrowed slices of `Arc`s because that is what [`crate::cache::Source`]
/// returns in both of its arms — the cache hands out shared objects and the
/// impersonated path wraps its own, so a builder over `&[Arc<K>]` never forces a
/// clone of the fleet.
pub struct GraphInputs<'a> {
    /// Namespaced repositories in scope.
    pub repositories: &'a [Arc<Repository>],
    /// Cluster repositories (always cluster-wide).
    pub cluster_repositories: &'a [Arc<ClusterRepository>],
    /// Policies whose repository refs become membership edges.
    pub policies: &'a [Arc<SnapshotPolicy>],
    /// Snapshot replications: repository-to-repository copy edges.
    pub snapshot_replications: &'a [Arc<SnapshotReplication>],
    /// Repository replications: repository-to-backend copy edges.
    pub repository_replications: &'a [Arc<RepositoryReplication>],
    /// Maintenance resources, which can degrade an otherwise healthy repository.
    pub maintenances: &'a [Arc<Maintenance>],
    /// The server's clock, stamped on the graph so the SPA can age it.
    pub now: DateTime<Utc>,
}

/// **Pure.** Assemble the whole topology.
///
/// Nodes are emitted before edges so every edge can be checked against the node
/// set; an edge endpoint that is still missing at that point mints a ghost node
/// rather than being silently dropped.
pub fn build(inputs: &GraphInputs) -> RepositoryGraph {
    let mut nodes: BTreeMap<String, GraphNode> = BTreeMap::new();
    let mut edges: Vec<GraphEdge> = Vec::new();

    for repo in inputs.repositories {
        let node = repository_node(repo, inputs.maintenances);
        nodes.insert(node.id.clone(), node);
    }
    for repo in inputs.cluster_repositories {
        let node = cluster_repository_node(repo, inputs.maintenances);
        nodes.insert(node.id.clone(), node);
    }
    for policy in inputs.policies {
        let node = policy_node(policy);
        nodes.insert(node.id.clone(), node);
    }

    for repo in inputs.cluster_repositories {
        allowed_namespace_edges(repo, &mut nodes, &mut edges);
    }
    for policy in inputs.policies {
        policy_membership_edges(policy, &mut nodes, &mut edges);
    }
    for repo in inputs.repositories {
        seed_edge(
            repo.spec.seed.as_ref().and_then(seed_repository_ref),
            repo.metadata.namespace.as_deref().unwrap_or_default(),
            &repository_id(repo),
            repo.status
                .as_ref()
                .and_then(|s| s.seed.as_ref())
                .is_some_and(|s| s.seeded_at.is_some()),
            &mut nodes,
            &mut edges,
        );
    }
    for repo in inputs.cluster_repositories {
        seed_edge(
            repo.spec.seed.as_ref().and_then(seed_repository_ref),
            "",
            &cluster_repository_id(repo),
            repo.status
                .as_ref()
                .and_then(|s| s.seed.as_ref())
                .is_some_and(|s| s.seeded_at.is_some()),
            &mut nodes,
            &mut edges,
        );
    }
    for repl in inputs.snapshot_replications {
        snapshot_replication_edge(repl, &mut nodes, &mut edges);
    }
    for repl in inputs.repository_replications {
        repository_replication_edge(repl, &mut nodes, &mut edges);
    }

    RepositoryGraph {
        nodes: nodes.into_values().collect(),
        edges,
        generated_at: inputs.now.to_rfc3339(),
    }
}

/// **Pure.** The colour of a repository node.
///
/// Precedence, most decisive first: a suspended repository is `Suspended`
/// whatever its phase says (the user asked for it, so it is not a fault); a
/// `Fail`-severity structural gate is `Failed` even at `phase: Ready`, because
/// the gate is exactly the case where the phase looks fine and the work is
/// wedged (#359); otherwise the phase decides. Exhaustive over
/// [`RepositoryPhase`], so a new phase cannot ship without a colour.
pub fn repository_health(
    phase: Option<&RepositoryPhase>,
    suspended: bool,
    gates: &[GateHit],
) -> Health {
    if suspended {
        return Health::Suspended;
    }
    if gates.iter().any(|g| g.severity == "Fail") {
        return Health::Failed;
    }
    match phase {
        Some(RepositoryPhase::Ready) => Health::Healthy,
        Some(RepositoryPhase::Degraded) => Health::Degraded,
        Some(RepositoryPhase::Failed) => Health::Failed,
        Some(RepositoryPhase::Pending) | Some(RepositoryPhase::Initializing) => Health::Pending,
        // A phase this build cannot interpret is never reported as ready.
        Some(RepositoryPhase::Unknown(_)) => Health::Unknown,
        None => Health::Unknown,
    }
}

/// **Pure.** The colour of a replication edge, given its projected phase and
/// whether the user suspended it.
fn replication_edge_health(
    phase: Option<&kopiur_ui_model::views::ReplicationPhaseView>,
    suspended: bool,
) -> Health {
    use kopiur_ui_model::views::ReplicationPhaseView as P;
    if suspended {
        return Health::Suspended;
    }
    match phase {
        Some(P::Succeeded) => Health::Healthy,
        Some(P::Replicating) | Some(P::Pending) => Health::Pending,
        Some(P::Failed) => Health::Failed,
        Some(P::Suspended) => Health::Suspended,
        Some(P::Unknown { .. }) => Health::Unknown,
        None => Health::Unknown,
    }
}

/// The node id of a namespaced repository — its [`repo_key`].
fn repository_id(repo: &Repository) -> String {
    format!(
        "Repository/{}/{}",
        repo.metadata.namespace.as_deref().unwrap_or_default(),
        repo.metadata.name.as_deref().unwrap_or_default()
    )
}

/// The node id of a cluster repository.
fn cluster_repository_id(repo: &ClusterRepository) -> String {
    format!(
        "ClusterRepository/{}",
        repo.metadata.name.as_deref().unwrap_or_default()
    )
}

/// The node id of a policy.
fn policy_id(policy: &SnapshotPolicy) -> String {
    format!(
        "Policy/{}/{}",
        policy.metadata.namespace.as_deref().unwrap_or_default(),
        policy.metadata.name.as_deref().unwrap_or_default()
    )
}

/// Whether a `Maintenance` covering this repository is failing a run track.
///
/// A repository whose compaction has been failing is working but wrong, which is
/// what [`Health::Degraded`] means — and it is invisible in the repository's own
/// phase, because maintenance runs on its own object.
fn maintenance_failing(
    maintenances: &[Arc<Maintenance>],
    kind: RepositoryKind,
    name: &str,
) -> bool {
    maintenances
        .iter()
        .filter(|m| kopiur_ops::maintenance::covers_repository(m, kind, name))
        .any(|m| {
            let status = m.status.as_ref();
            let failing = |track: Option<&kopiur_api::maintenance::RunStatus>| {
                track.and_then(|t| t.consecutive_failures).unwrap_or(0) > 0
            };
            failing(status.and_then(|s| s.quick.as_ref()))
                || failing(status.and_then(|s| s.full.as_ref()))
        })
}

/// Downgrade a healthy node when its maintenance is failing; leave every other
/// colour alone (a failed repository does not become "degraded").
fn with_maintenance(health: Health, failing: bool) -> Health {
    match (health, failing) {
        (Health::Healthy, true) => Health::Degraded,
        (other, _) => other,
    }
}

/// The graph node for one namespaced repository.
fn repository_node(repo: &Repository, maintenances: &[Arc<Maintenance>]) -> GraphNode {
    let name = repo.metadata.name.clone().unwrap_or_default();
    let namespace = repo.metadata.namespace.clone();
    let status = repo.status.as_ref();
    let gates = gate_hits(
        status.map(|s| s.conditions.as_slice()).unwrap_or_default(),
        GateScope::covers_repository,
    );
    let health = with_maintenance(
        repository_health(
            status.and_then(|s| s.phase.as_ref()),
            repo.spec.suspend,
            &gates,
        ),
        maintenance_failing(maintenances, RepositoryKind::Repository, &name),
    );
    GraphNode {
        id: repository_id(repo),
        kind: NodeKind::Repository,
        label: format!("{}/{name}", namespace.as_deref().unwrap_or_default()),
        name,
        namespace,
        health,
        missing: false,
        allows_all_namespaces: false,
        backend_kind: Some(repo.spec.backend.kind_str().to_string()),
        gates,
    }
}

/// The graph node for one cluster repository.
fn cluster_repository_node(
    repo: &ClusterRepository,
    maintenances: &[Arc<Maintenance>],
) -> GraphNode {
    let name = repo.metadata.name.clone().unwrap_or_default();
    let status = repo.status.as_ref();
    let gates = gate_hits(
        status.map(|s| s.conditions.as_slice()).unwrap_or_default(),
        GateScope::covers_repository,
    );
    let health = with_maintenance(
        repository_health(
            status.and_then(|s| s.phase.as_ref()),
            repo.spec.suspend,
            &gates,
        ),
        maintenance_failing(maintenances, RepositoryKind::ClusterRepository, &name),
    );
    GraphNode {
        id: cluster_repository_id(repo),
        kind: NodeKind::ClusterRepository,
        label: name.clone(),
        name,
        namespace: None,
        health,
        missing: false,
        allows_all_namespaces: allows_all(&repo.spec.allowed_namespaces),
        backend_kind: Some(repo.spec.backend.kind_str().to_string()),
        gates,
    }
}

/// Whether `spec.allowedNamespaces` admits every namespace. Exhaustive.
fn allows_all(allowed: &AllowedNamespaces) -> bool {
    match allowed {
        AllowedNamespaces::All(yes) => *yes,
        AllowedNamespaces::List(_) | AllowedNamespaces::Selector(_) => false,
    }
}

/// The graph node for one policy.
fn policy_node(policy: &SnapshotPolicy) -> GraphNode {
    let name = policy.metadata.name.clone().unwrap_or_default();
    let namespace = policy.metadata.namespace.clone();
    let gates = gate_hits(
        policy
            .status
            .as_ref()
            .map(|s| s.conditions.as_slice())
            .unwrap_or_default(),
        GateScope::covers_snapshot_policy,
    );
    let health = if policy.spec.suspend {
        Health::Suspended
    } else if gates.iter().any(|g| g.severity == "Fail") {
        Health::Failed
    } else {
        Health::Healthy
    };
    GraphNode {
        id: policy_id(policy),
        kind: NodeKind::Policy,
        label: format!("{}/{name}", namespace.as_deref().unwrap_or_default()),
        name,
        namespace,
        health,
        missing: false,
        allows_all_namespaces: false,
        backend_kind: None,
        gates,
    }
}

/// A ghost node for a reference that resolves to nothing.
///
/// `Failed` rather than `Unknown`: a reference to an object that is not there
/// will never reconcile, and drawing it grey would read as "not observed yet".
fn missing_node(id: &str, kind: NodeKind) -> GraphNode {
    let (name, namespace) = match id.split('/').collect::<Vec<_>>().as_slice() {
        [_, ns, name] => ((*name).to_string(), Some((*ns).to_string())),
        [_, name] => ((*name).to_string(), None),
        _ => (id.to_string(), None),
    };
    GraphNode {
        id: id.to_string(),
        kind,
        label: format!("{name} (missing)"),
        name,
        namespace,
        health: Health::Failed,
        missing: true,
        allows_all_namespaces: false,
        backend_kind: None,
        gates: Vec::new(),
    }
}

/// Resolve a repository ref to a node id, minting a ghost node when nothing in
/// the graph answers to it.
fn resolve_repository(
    r: &RepositoryRef,
    owner_ns: &str,
    nodes: &mut BTreeMap<String, GraphNode>,
) -> String {
    let id = repo_key(r, owner_ns);
    if !nodes.contains_key(&id) {
        let kind = match r.kind {
            RepositoryKind::Repository => NodeKind::Repository,
            RepositoryKind::ClusterRepository => NodeKind::ClusterRepository,
        };
        nodes.insert(id.clone(), missing_node(&id, kind));
    }
    id
}

/// The non-secret location a backend points at, for a backend node's label.
///
/// Buckets, paths and hosts only — never a credential, never a `Secret`
/// reference. Exhaustive over [`Backend`], so a new backend cannot ship without
/// someone deciding what of it is safe to draw.
fn backend_location(backend: &Backend) -> String {
    match backend {
        Backend::S3(b) => format!("{}/{}", b.bucket, b.prefix.clone().unwrap_or_default()),
        Backend::Azure(b) => format!("{}/{}", b.container, b.prefix.clone().unwrap_or_default()),
        Backend::Gcs(b) => format!("{}/{}", b.bucket, b.prefix.clone().unwrap_or_default()),
        Backend::B2(b) => format!("{}/{}", b.bucket, b.prefix.clone().unwrap_or_default()),
        Backend::Filesystem(b) => b.path.clone(),
        Backend::Sftp(b) => format!("{}:{}", b.host, b.path),
        Backend::WebDav(b) => b.url.clone(),
        Backend::Rclone(b) => b.remote_path.clone(),
        Backend::Gdrive(b) => b.folder_id.clone(),
    }
}

/// The `Backend/<repl-ns>/<repl-name>` node a repository replication writes to.
///
/// Keyed by the replication rather than by the backend's own coordinates because
/// a bare destination backend is not an object with an identity — two
/// replications naming the same bucket are two declarations, and collapsing them
/// into one node would claim a relationship the cluster does not record.
fn backend_node(repl: &RepositoryReplication) -> GraphNode {
    let name = repl.metadata.name.clone().unwrap_or_default();
    let namespace = repl.metadata.namespace.clone().unwrap_or_default();
    let kind = repl.spec.destination.kind_str().to_string();
    let location = backend_location(&repl.spec.destination);
    GraphNode {
        id: format!("Backend/{namespace}/{name}"),
        kind: NodeKind::Backend,
        label: if location.is_empty() {
            kind.clone()
        } else {
            format!("{kind} {location}")
        },
        name,
        namespace: Some(namespace),
        health: Health::Unknown,
        missing: false,
        allows_all_namespaces: false,
        backend_kind: Some(kind),
        gates: Vec::new(),
    }
}

/// The `AllowedNamespace` edges out of a cluster repository.
///
/// A `List` fans out to one `Namespace/<name>` node each; a `Selector` gets one
/// `NamespaceSelector/<cr-name>` node standing for whatever it matches (the UI
/// does not list namespaces, so enumerating them would be a second read the
/// caller may not even be allowed to make); `All` draws no edge at all, because
/// `allows_all_namespaces` on the node already says it and an edge to every
/// namespace in the cluster is not a picture. Exhaustive.
fn allowed_namespace_edges(
    repo: &ClusterRepository,
    nodes: &mut BTreeMap<String, GraphNode>,
    edges: &mut Vec<GraphEdge>,
) {
    let from = cluster_repository_id(repo);
    let cr_name = repo.metadata.name.clone().unwrap_or_default();
    match &repo.spec.allowed_namespaces {
        AllowedNamespaces::List(names) => {
            for ns in names {
                let id = format!("Namespace/{ns}");
                nodes.entry(id.clone()).or_insert_with(|| GraphNode {
                    id: id.clone(),
                    kind: NodeKind::Namespace,
                    name: ns.clone(),
                    namespace: None,
                    label: ns.clone(),
                    health: Health::Healthy,
                    missing: false,
                    allows_all_namespaces: false,
                    backend_kind: None,
                    gates: Vec::new(),
                });
                edges.push(GraphEdge {
                    id: format!("AllowedNamespace/{from}/{id}"),
                    from: from.clone(),
                    to: id,
                    kind: EdgeKind::AllowedNamespace,
                    label: None,
                    health: Health::Healthy,
                });
            }
        }
        AllowedNamespaces::Selector(sel) => {
            let id = format!("NamespaceSelector/{cr_name}");
            let rendered = label_selector_string(sel);
            nodes.entry(id.clone()).or_insert_with(|| GraphNode {
                id: id.clone(),
                kind: NodeKind::NamespaceSelector,
                name: cr_name.clone(),
                namespace: None,
                label: rendered.clone(),
                health: Health::Healthy,
                missing: false,
                allows_all_namespaces: false,
                backend_kind: None,
                gates: Vec::new(),
            });
            edges.push(GraphEdge {
                id: format!("AllowedNamespace/{from}/{id}"),
                from: from.clone(),
                to: id,
                kind: EdgeKind::AllowedNamespace,
                label: Some(rendered),
                health: Health::Healthy,
            });
        }
        AllowedNamespaces::All(_) => {}
    }
}

/// One `PolicyMembership` edge per repository a policy names.
fn policy_membership_edges(
    policy: &SnapshotPolicy,
    nodes: &mut BTreeMap<String, GraphNode>,
    edges: &mut Vec<GraphEdge>,
) {
    let from = policy_id(policy);
    let owner_ns = policy.metadata.namespace.clone().unwrap_or_default();
    for r in repository_refs(&policy.spec) {
        let to = resolve_repository(r, &owner_ns, nodes);
        let health = edge_health_for(nodes, &to, Health::Healthy);
        edges.push(GraphEdge {
            id: format!("PolicyMembership/{from}/{to}"),
            from: from.clone(),
            to,
            kind: EdgeKind::PolicyMembership,
            label: None,
            health,
        });
    }
}

/// A `Seed` edge from the repository this one was bootstrapped from.
///
/// Only migrate-mode seeds draw one: a blob-mode seed reads a bare backend,
/// which is not a node in this graph.
fn seed_edge(
    source: Option<&RepositoryRef>,
    owner_ns: &str,
    to: &str,
    seeded: bool,
    nodes: &mut BTreeMap<String, GraphNode>,
    edges: &mut Vec<GraphEdge>,
) {
    let Some(source) = source else { return };
    let from = resolve_repository(source, owner_ns, nodes);
    let health = edge_health_for(
        nodes,
        &from,
        if seeded {
            Health::Healthy
        } else {
            Health::Pending
        },
    );
    edges.push(GraphEdge {
        id: format!("Seed/{from}/{to}"),
        from,
        to: to.to_string(),
        kind: EdgeKind::Seed,
        label: None,
        health,
    });
}

/// The repository-to-repository copy edge of one `SnapshotReplication`.
fn snapshot_replication_edge(
    repl: &SnapshotReplication,
    nodes: &mut BTreeMap<String, GraphNode>,
    edges: &mut Vec<GraphEdge>,
) {
    let owner_ns = repl.metadata.namespace.clone().unwrap_or_default();
    let from = resolve_repository(&repl.spec.source_ref, &owner_ns, nodes);
    let to = resolve_repository(&repl.spec.destination_ref, &owner_ns, nodes);
    let phase = repl
        .status
        .as_ref()
        .and_then(|s| s.phase.as_ref())
        .map(snapshot_replication_phase_view);
    let health = replication_edge_health(phase.as_ref(), repl.spec.suspend);
    edges.push(GraphEdge {
        id: format!(
            "SnapshotReplication/{owner_ns}/{}",
            repl.metadata.name.as_deref().unwrap_or_default()
        ),
        from,
        to,
        kind: EdgeKind::SnapshotReplication,
        label: Some(repl.spec.schedule.cron.clone()),
        health,
    });
}

/// The repository-to-backend copy edge of one `RepositoryReplication`.
fn repository_replication_edge(
    repl: &RepositoryReplication,
    nodes: &mut BTreeMap<String, GraphNode>,
    edges: &mut Vec<GraphEdge>,
) {
    let owner_ns = repl.metadata.namespace.clone().unwrap_or_default();
    let from = resolve_repository(&repl.spec.source_ref, &owner_ns, nodes);
    let destination = backend_node(repl);
    let to = destination.id.clone();
    nodes.entry(to.clone()).or_insert(destination);
    let phase = repl
        .status
        .as_ref()
        .and_then(|s| s.phase.as_ref())
        .map(repository_replication_phase_view);
    let health = replication_edge_health(phase.as_ref(), repl.spec.suspend);
    edges.push(GraphEdge {
        id: format!(
            "RepositoryReplication/{owner_ns}/{}",
            repl.metadata.name.as_deref().unwrap_or_default()
        ),
        from,
        to,
        kind: EdgeKind::RepositoryReplication,
        label: Some(repl.spec.schedule.cron.clone()),
        health,
    });
}

/// An edge into a ghost node is failed, whatever it would otherwise have been:
/// the relationship cannot work if one end is not there.
fn edge_health_for(
    nodes: &BTreeMap<String, GraphNode>,
    endpoint: &str,
    otherwise: Health,
) -> Health {
    match nodes.get(endpoint) {
        Some(node) if node.missing => Health::Failed,
        Some(_) | None => otherwise,
    }
}

/// Read every object the graph is built from, under the caller's identity.
async fn load(
    app: &AppState,
    id: &crate::auth::identity::Identity,
    namespace: Option<&str>,
) -> Result<RepositoryGraph, ApiError> {
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
    let snapshot_replications = app
        .source
        .list::<SnapshotReplication>(id, &client, namespace)
        .await?;
    let repository_replications = app
        .source
        .list::<RepositoryReplication>(id, &client, namespace)
        .await?;
    let maintenances = app
        .source
        .list::<Maintenance>(id, &client, namespace)
        .await?;

    Ok(build(&GraphInputs {
        repositories: &repositories,
        cluster_repositories: &cluster_repositories,
        policies: &policies,
        snapshot_replications: &snapshot_replications,
        repository_replications: &repository_replications,
        maintenances: &maintenances,
        now: Utc::now(),
    }))
}

/// `GET /api/v1/graph`
async fn handler(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    axum::extract::Query(q): axum::extract::Query<NamespaceQuery>,
) -> Result<Json<RepositoryGraph>, ApiError> {
    Ok(Json(load(&app, &id, q.namespace.as_deref()).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::testutil::from_yaml;

    /// The fixture cluster: two namespaced repositories (one seeded from the
    /// other, one wearing a phase this build has never heard of), three cluster
    /// repositories covering all three `allowedNamespaces` shapes, a
    /// snapshot replication, a repository replication to a bare S3 destination,
    /// a multi-repository policy and a policy whose ref dangles.
    struct Fixture {
        repositories: Vec<Arc<Repository>>,
        cluster_repositories: Vec<Arc<ClusterRepository>>,
        policies: Vec<Arc<SnapshotPolicy>>,
        snapshot_replications: Vec<Arc<SnapshotReplication>>,
        repository_replications: Vec<Arc<RepositoryReplication>>,
    }

    fn fixture() -> Fixture {
        let primary: Repository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: { name: nas, namespace: media }
spec:
  backend: { filesystem: { path: /repo } }
  encryption: { passwordSecretRef: { name: nas-pw, key: password } }
status:
  phase: Ready
"#,
        );
        // Seeded from `nas`, and parked on a phase this build does not know.
        let mirror: Repository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: { name: mirror, namespace: media }
spec:
  backend: { s3: { bucket: mirror-bucket, prefix: kopiur/ } }
  encryption: { passwordSecretRef: { name: mirror-pw, key: password } }
  seed:
    from:
      repository: { kind: Repository, name: nas }
status:
  phase: Weird
  seed: { seededAt: "2026-09-01T00:00:00Z" }
"#,
        );
        let listed: ClusterRepository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: ClusterRepository
metadata: { name: shared-list }
spec:
  backend: { s3: { bucket: shared } }
  encryption: { passwordSecretRef: { name: shared-pw, key: password } }
  allowedNamespaces: { list: [prod, staging] }
status:
  phase: Ready
"#,
        );
        let selected: ClusterRepository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: ClusterRepository
metadata: { name: shared-selector }
spec:
  backend: { s3: { bucket: shared2 } }
  encryption: { passwordSecretRef: { name: shared-pw, key: password } }
  allowedNamespaces:
    selector: { matchLabels: { backup: "yes" } }
status:
  phase: Degraded
"#,
        );
        let everywhere: ClusterRepository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: ClusterRepository
metadata: { name: shared-all }
spec:
  backend: { s3: { bucket: shared3 } }
  encryption: { passwordSecretRef: { name: shared-pw, key: password } }
  allowedNamespaces: { all: true }
status:
  phase: Ready
"#,
        );
        let multi: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: media }
spec:
  repositories:
    - { kind: Repository, name: nas }
    - { kind: ClusterRepository, name: shared-list }
  sources:
    - pvc: { name: data }
"#,
        );
        let dangling: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: orphaned, namespace: media }
spec:
  repository: { kind: Repository, name: gone }
  sources:
    - pvc: { name: data }
"#,
        );
        let snap_repl: SnapshotReplication = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotReplication
metadata: { name: offsite, namespace: media }
spec:
  sourceRef: { kind: Repository, name: nas }
  destinationRef: { kind: ClusterRepository, name: shared-list }
  schedule: { cron: "0 3 * * *" }
status:
  phase: Succeeded
"#,
        );
        let repo_repl: RepositoryReplication = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: RepositoryReplication
metadata: { name: blobsync, namespace: media }
spec:
  sourceRef: { kind: Repository, name: nas }
  destination: { s3: { bucket: dr-bucket, prefix: nas/ } }
  schedule: { cron: "0 5 * * *" }
status:
  phase: Failed
"#,
        );
        Fixture {
            repositories: vec![Arc::new(primary), Arc::new(mirror)],
            cluster_repositories: vec![Arc::new(listed), Arc::new(selected), Arc::new(everywhere)],
            policies: vec![Arc::new(multi), Arc::new(dangling)],
            snapshot_replications: vec![Arc::new(snap_repl)],
            repository_replications: vec![Arc::new(repo_repl)],
        }
    }

    fn built() -> RepositoryGraph {
        let f = fixture();
        build(&GraphInputs {
            repositories: &f.repositories,
            cluster_repositories: &f.cluster_repositories,
            policies: &f.policies,
            snapshot_replications: &f.snapshot_replications,
            repository_replications: &f.repository_replications,
            maintenances: &[],
            now: DateTime::parse_from_rfc3339("2026-09-08T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        })
    }

    #[test]
    fn the_node_set_is_exactly_the_objects_plus_their_synthetic_peers() {
        let graph = built();
        let mut ids: Vec<&str> = graph.nodes.iter().map(|n| n.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                "Backend/media/blobsync",
                "ClusterRepository/shared-all",
                "ClusterRepository/shared-list",
                "ClusterRepository/shared-selector",
                "Namespace/prod",
                "Namespace/staging",
                "NamespaceSelector/shared-selector",
                "Policy/media/nightly",
                "Policy/media/orphaned",
                "Repository/media/gone",
                "Repository/media/mirror",
                "Repository/media/nas",
            ]
        );
    }

    #[test]
    fn the_edge_set_joins_them_as_the_objects_declare() {
        let graph = built();
        let mut ids: Vec<&str> = graph.edges.iter().map(|e| e.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                "AllowedNamespace/ClusterRepository/shared-list/Namespace/prod",
                "AllowedNamespace/ClusterRepository/shared-list/Namespace/staging",
                "AllowedNamespace/ClusterRepository/shared-selector/NamespaceSelector/shared-selector",
                "PolicyMembership/Policy/media/nightly/ClusterRepository/shared-list",
                "PolicyMembership/Policy/media/nightly/Repository/media/nas",
                "PolicyMembership/Policy/media/orphaned/Repository/media/gone",
                "RepositoryReplication/media/blobsync",
                "Seed/Repository/media/nas/Repository/media/mirror",
                "SnapshotReplication/media/offsite",
            ]
        );
    }

    #[test]
    fn an_all_namespaces_cluster_repository_draws_a_flag_not_an_edge() {
        let graph = built();
        let all = graph
            .nodes
            .iter()
            .find(|n| n.id == "ClusterRepository/shared-all")
            .expect("the all-namespaces repository is a node");
        assert!(all.allows_all_namespaces);
        assert!(
            !graph.edges.iter().any(|e| e.from == all.id),
            "an edge to every namespace in the cluster is not a picture"
        );
        let listed = graph
            .nodes
            .iter()
            .find(|n| n.id == "ClusterRepository/shared-list")
            .unwrap();
        assert!(!listed.allows_all_namespaces);
    }

    #[test]
    fn a_dangling_repository_reference_becomes_a_failed_ghost() {
        let graph = built();
        let ghost = graph
            .nodes
            .iter()
            .find(|n| n.id == "Repository/media/gone")
            .expect("a policy pointing at nothing still draws the nothing");
        assert!(ghost.missing);
        assert_eq!(ghost.health, Health::Failed);
        assert_eq!(ghost.kind, NodeKind::Repository);

        let edge = graph
            .edges
            .iter()
            .find(|e| e.to == "Repository/media/gone")
            .unwrap();
        assert_eq!(
            edge.health,
            Health::Failed,
            "an edge into a ghost is failed however healthy its source is"
        );
    }

    #[test]
    fn node_health_follows_phase_and_the_unknown_phase_is_unknown() {
        let graph = built();
        let by_id = |id: &str| {
            graph
                .nodes
                .iter()
                .find(|n| n.id == id)
                .unwrap_or_else(|| panic!("{id} is a node"))
        };
        assert_eq!(by_id("Repository/media/nas").health, Health::Healthy);
        assert_eq!(
            by_id("Repository/media/mirror").health,
            Health::Unknown,
            "`phase: Weird` must degrade to Unknown, never to Ready"
        );
        assert_eq!(
            by_id("ClusterRepository/shared-selector").health,
            Health::Degraded
        );
    }

    #[test]
    fn replication_edges_carry_their_cron_and_their_run_health() {
        let graph = built();
        let snap = graph
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::SnapshotReplication)
            .unwrap();
        assert_eq!(snap.from, "Repository/media/nas");
        assert_eq!(snap.to, "ClusterRepository/shared-list");
        assert_eq!(snap.label.as_deref(), Some("0 3 * * *"));
        assert_eq!(snap.health, Health::Healthy);

        let blob = graph
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::RepositoryReplication)
            .unwrap();
        assert_eq!(blob.from, "Repository/media/nas");
        assert_eq!(blob.to, "Backend/media/blobsync");
        assert_eq!(blob.health, Health::Failed, "a failed run is a failed edge");
    }

    #[test]
    fn a_bare_destination_backend_is_labelled_with_its_location_and_no_secret() {
        let graph = built();
        let backend = graph
            .nodes
            .iter()
            .find(|n| n.id == "Backend/media/blobsync")
            .unwrap();
        assert_eq!(backend.kind, NodeKind::Backend);
        assert_eq!(backend.backend_kind.as_deref(), Some("S3"));
        assert_eq!(backend.label, "S3 dr-bucket/nas/");
        assert!(
            !backend.label.contains("Secret") && !backend.label.contains("password"),
            "a backend label carries location only: {}",
            backend.label
        );
    }

    #[test]
    fn a_namespace_selector_renders_the_selector_it_stands_for() {
        let graph = built();
        let selector = graph
            .nodes
            .iter()
            .find(|n| n.id == "NamespaceSelector/shared-selector")
            .unwrap();
        assert_eq!(selector.kind, NodeKind::NamespaceSelector);
        assert_eq!(selector.label, "backup=yes");
    }

    #[test]
    fn the_graph_is_stamped_with_the_clock_it_was_built_at() {
        let graph = built();
        assert!(
            graph.generated_at.starts_with("2026-09-08T12:00:00"),
            "got {}",
            graph.generated_at
        );
    }

    #[test]
    fn repository_health_is_a_total_table() {
        let fail_gate = vec![GateHit {
            condition: "DeletionHeld".into(),
            reason: "DeletionProtectionEngaged".into(),
            severity: "Fail".into(),
            message: "held".into(),
        }];
        let warn_gate = vec![GateHit {
            condition: "MoverPermitted".into(),
            reason: "PrivilegedMoverNotPermitted".into(),
            severity: "Warn".into(),
            message: "not opted in".into(),
        }];

        assert_eq!(
            repository_health(Some(&RepositoryPhase::Ready), false, &[]),
            Health::Healthy
        );
        assert_eq!(
            repository_health(Some(&RepositoryPhase::Degraded), false, &[]),
            Health::Degraded
        );
        assert_eq!(
            repository_health(Some(&RepositoryPhase::Failed), false, &[]),
            Health::Failed
        );
        assert_eq!(
            repository_health(Some(&RepositoryPhase::Pending), false, &[]),
            Health::Pending
        );
        assert_eq!(
            repository_health(Some(&RepositoryPhase::Initializing), false, &[]),
            Health::Pending
        );
        assert_eq!(
            repository_health(Some(&RepositoryPhase::Unknown("Weird".into())), false, &[]),
            Health::Unknown
        );
        assert_eq!(repository_health(None, false, &[]), Health::Unknown);

        // Suspension wins over everything: the user asked for it.
        assert_eq!(
            repository_health(Some(&RepositoryPhase::Ready), true, &fail_gate),
            Health::Suspended
        );
        // A failing gate wins over a Ready phase — that pairing IS #359.
        assert_eq!(
            repository_health(Some(&RepositoryPhase::Ready), false, &fail_gate),
            Health::Failed
        );
        // A warning gate does not, on its own, turn a node red.
        assert_eq!(
            repository_health(Some(&RepositoryPhase::Ready), false, &warn_gate),
            Health::Healthy
        );
    }

    #[test]
    fn a_failing_maintenance_degrades_an_otherwise_healthy_repository() {
        let repos = fixture().repositories;
        let maint: Maintenance = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Maintenance
metadata: { name: nas-maint, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  schedule:
    quick: { cron: "0 * * * *" }
    full: { cron: "0 4 * * 0" }
  ownership: { owner: kopiur/media/nas }
status:
  full: { consecutiveFailures: 3 }
"#,
        );
        let maintenances = vec![Arc::new(maint)];
        let graph = build(&GraphInputs {
            repositories: &repos,
            cluster_repositories: &[],
            policies: &[],
            snapshot_replications: &[],
            repository_replications: &[],
            maintenances: &maintenances,
            now: Utc::now(),
        });
        let nas = graph
            .nodes
            .iter()
            .find(|n| n.id == "Repository/media/nas")
            .unwrap();
        assert_eq!(
            nas.health,
            Health::Degraded,
            "a repository whose compaction keeps failing is not healthy, however Ready it looks"
        );
        // The unknown-phase repository is not "upgraded" by an unrelated
        // maintenance: only Healthy is downgraded.
        let mirror = graph
            .nodes
            .iter()
            .find(|n| n.id == "Repository/media/mirror")
            .unwrap();
        assert_eq!(mirror.health, Health::Unknown);
    }

    #[test]
    fn an_unfinished_seed_is_a_pending_edge() {
        let unseeded: Repository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: { name: restoring, namespace: media }
spec:
  backend: { filesystem: { path: /restore } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
  seed:
    from:
      repository: { kind: Repository, name: nas }
"#,
        );
        let mut all = fixture().repositories;
        all.push(Arc::new(unseeded));
        let graph = build(&GraphInputs {
            repositories: &all,
            cluster_repositories: &[],
            policies: &[],
            snapshot_replications: &[],
            repository_replications: &[],
            maintenances: &[],
            now: Utc::now(),
        });
        let edge = graph
            .edges
            .iter()
            .find(|e| e.to == "Repository/media/restoring")
            .expect("an unfinished seed still draws its provenance");
        assert_eq!(edge.kind, EdgeKind::Seed);
        assert_eq!(edge.health, Health::Pending);
    }

    #[test]
    fn a_blob_mode_seed_draws_no_edge_because_a_bare_backend_is_not_a_node() {
        let blob_seeded: Repository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: { name: fromblob, namespace: media }
spec:
  backend: { filesystem: { path: /repo } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
  seed:
    from:
      backend: { s3: { bucket: old-backups } }
"#,
        );
        let repos = vec![Arc::new(blob_seeded)];
        let graph = build(&GraphInputs {
            repositories: &repos,
            cluster_repositories: &[],
            policies: &[],
            snapshot_replications: &[],
            repository_replications: &[],
            maintenances: &[],
            now: Utc::now(),
        });
        assert!(
            graph.edges.is_empty(),
            "a blob seed reads storage, not another repository: {:?}",
            graph.edges
        );
    }

    #[test]
    fn every_backend_variant_has_a_non_secret_location() {
        let cases = [
            (r#"{ "s3": { "bucket": "b", "prefix": "p/" } }"#, "b/p/"),
            (r#"{ "azure": { "container": "c" } }"#, "c/"),
            (r#"{ "gcs": { "bucket": "g" } }"#, "g/"),
            (r#"{ "b2": { "bucket": "b2b" } }"#, "b2b/"),
            (r#"{ "filesystem": { "path": "/repo" } }"#, "/repo"),
            (r#"{ "sftp": { "host": "h", "path": "/p" } }"#, "h:/p"),
            (
                r#"{ "webDav": { "url": "https://dav/x" } }"#,
                "https://dav/x",
            ),
            (
                r#"{ "rclone": { "remotePath": "rem:bucket" } }"#,
                "rem:bucket",
            ),
            (r#"{ "gdrive": { "folderId": "fid" } }"#, "fid"),
        ];
        for (json, expected) in cases {
            let backend: Backend = serde_json::from_str(json).expect(json);
            assert_eq!(backend_location(&backend), expected, "for {json}");
        }
    }
}
