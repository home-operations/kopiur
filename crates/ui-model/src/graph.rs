//! The repository topology graph: repositories, their backends, the policies
//! that write to them, and the replication edges between them.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// What a [`GraphNode`] stands for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum NodeKind {
    /// A namespaced `Repository`.
    Repository,
    /// A cluster-scoped `ClusterRepository`.
    ClusterRepository,
    /// The storage backend a repository points at (S3, filesystem, …), drawn as
    /// its own node so two repositories sharing storage are visibly joined.
    Backend,
    /// A `SnapshotPolicy`.
    Policy,
    /// A namespace a cluster repository admits.
    Namespace,
    /// A label selector standing in for the set of namespaces it matches, drawn
    /// when enumerating them individually would swamp the graph.
    NamespaceSelector,
}

/// What a [`GraphEdge`] stands for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum EdgeKind {
    /// A `SnapshotReplication` copying selected snapshots between repositories.
    SnapshotReplication,
    /// A `RepositoryReplication` copying the whole repository's blobs.
    RepositoryReplication,
    /// A seed relationship: the destination repository was bootstrapped from
    /// the source.
    Seed,
    /// A `SnapshotPolicy` writing into a repository.
    PolicyMembership,
    /// A namespace (or selector) a cluster repository admits.
    AllowedNamespace,
}

/// Coarse health of a node or edge, driving its color in the graph and its
/// badge in list views.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum Health {
    /// Reconciled and ready.
    Healthy,
    /// Working, but something is wrong — a probe failure, a retrying run.
    Degraded,
    /// Terminally failed and needs attention.
    Failed,
    /// Deliberately suspended by the user.
    Suspended,
    /// Not reconciled yet.
    Pending,
    /// No status to read, or a phase this build does not recognize.
    Unknown,
}

/// How loudly a gate should be reported.
///
/// A closed enum rather than a string because the severity is *load-bearing on
/// both ends*: the SPA styles a banner from it, and the backend decides a
/// node's [`Health`] from it. A stringly-typed severity let one half of the
/// operator's own gate registry ship `Fail` while the other shipped `error`,
/// with nothing to catch it — the two projections now cannot disagree because
/// there is only one type to project onto.
///
/// Mirrors `kopiur_api::gates::GateSeverity`, which has exactly these two
/// levels: a gate is either wedged work or a refusal that may well be the
/// configuration you asked for. There is no `info` — an informational gate
/// would not be a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum GateSeverityView {
    /// The block is refused work that may be a deliberate choice.
    Warning,
    /// Work is wedged and cannot progress without an out-of-band change.
    Error,
}

/// One admission or reconcile gate that is currently holding a resource back.
///
/// Mirrors the blocking `status.conditions` entry that produced it, flattened
/// into the four fields the UI renders as a warning banner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct GateHit {
    /// The `status.conditions[].type` that is gating, e.g. `Ready`.
    pub condition: String,
    /// The `status.conditions[].reason`, e.g. `DeletionProtectionEngaged`.
    pub reason: String,
    /// How serious the gate is.
    pub severity: GateSeverityView,
    /// The `status.conditions[].message`, shown verbatim.
    pub message: String,
}

/// One vertex of the repository topology graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct GraphNode {
    /// Stable identifier used by [`GraphEdge::from`]/[`GraphEdge::to`], unique
    /// within one [`RepositoryGraph`].
    pub id: String,
    /// What this node stands for.
    pub kind: NodeKind,
    /// `metadata.name` of the underlying object, or the backend/selector's
    /// canonical name for synthetic nodes.
    pub name: String,
    /// `metadata.namespace`, absent for cluster-scoped and synthetic nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Display label for the node, already disambiguated for rendering.
    pub label: String,
    /// Coarse health, from the object's conditions and phase.
    pub health: Health,
    /// True when the node is referenced by an edge but the object does not
    /// exist — a dangling reference, drawn as a ghost.
    pub missing: bool,
    /// For cluster repositories: true when `spec.allowedNamespaces` admits
    /// every namespace rather than an explicit set.
    pub allows_all_namespaces: bool,
    /// For backend nodes: which backend variant this is (`s3`, `filesystem`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_kind: Option<String>,
    /// Gates currently holding this node's object back.
    pub gates: Vec<GateHit>,
}

/// One directed relationship between two [`GraphNode`]s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct GraphEdge {
    /// Stable identifier, unique within one [`RepositoryGraph`].
    pub id: String,
    /// [`GraphNode::id`] this edge starts at.
    pub from: String,
    /// [`GraphNode::id`] this edge ends at.
    pub to: String,
    /// What the relationship is.
    pub kind: EdgeKind,
    /// Optional edge label, e.g. the replication's cron schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Coarse health of the relationship itself — a failing replication is a
    /// failed edge between two healthy nodes.
    pub health: Health,
}

/// The whole repository topology, as of one point in time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RepositoryGraph {
    /// Every vertex.
    pub nodes: Vec<GraphNode>,
    /// Every directed relationship between them.
    pub edges: Vec<GraphEdge>,
    /// RFC3339 timestamp of when the server assembled this graph.
    pub generated_at: String,
}
