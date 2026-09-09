//! The `/api/v1` surface: one module per resource family, plus the shared error
//! shape.
//!
//! Every handler splits into two halves. `load_*` does the IO — reading from the
//! reflector cache or through an impersonated client, filtered by what the caller
//! may see — and `view_*` is a pure function from Kubernetes objects to
//! `kopiur_ui_model` wire types, so the mapping is fixture-testable without a
//! cluster.
//!
//! The router itself is assembled in [`crate::app`].
//!
//! # What lives here rather than in a resource module
//!
//! The projections every module shares: the `*PhaseView` mappings, the condition
//! and repository-reference renderers, and [`gate_hits`]. They are here because
//! two modules rendering the same phase differently is exactly the drift the
//! `Unknown { raw }` variant exists to prevent — one mapping, one place, one set
//! of exhaustive `match`es.
//!
//! # Reads never touch `Api::list` for the nine CRDs
//!
//! Kopiur objects are read through [`crate::cache::Source`], which is the only
//! thing that knows whether this deployment gates reads with a
//! `SubjectAccessReview` or lets the apiserver do it on an impersonated request.
//! The three non-Kopiur reads the API needs — Kubernetes `Event`s,
//! `SelfSubjectAccessReview`, and the browse-session `Job` — go through the
//! impersonated client directly, because they have no store and no `KopiurKind`.

pub mod doctor;
pub mod events;
pub mod gates;
pub mod graph;
pub mod maintenance;
pub mod me;
pub mod policies;
pub mod problem;
pub mod replications;
pub mod repositories;
pub mod restores;
pub mod schedules;
pub mod snapshots;
pub mod status;

use axum::Router;
use axum::extract::{FromRequestParts, Path, Query};
use axum::http::request::Parts;
use serde::Deserialize;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};

use kopiur_api::common::{RepositoryKind, RepositoryRef, repo_key};
use kopiur_api::gates::{GateScope, GateSeverity, STRUCTURAL_GATES};
use kopiur_api::maintenance::ManualRunPhase;
use kopiur_api::repository_replication::RepositoryReplicationPhase;
use kopiur_api::restore::RestoreClaimPhase;
use kopiur_api::snapshot_replication::SnapshotReplicationPhase;
use kopiur_api::{Origin, RepositoryPhase, RestorePhase, SnapshotPhase};
use kopiur_ops::{OpsCtx, Scope};
use kopiur_ui_model::graph::{GateHit, GateSeverityView};
use kopiur_ui_model::views::{
    ConditionView, OriginView, Page, ReplicationPhaseView, RepositoryPhaseView, RestorePhaseView,
    SnapshotPhaseView,
};

use crate::AppState;
use crate::api::problem::{ApiError, problem, request_path};
use crate::auth::identity::Identity;
use crate::config::{FIELD_MANAGER, UiConfig};

/// The namespace fallback for a request that named none and a deployment that
/// configured no operator namespace.
///
/// Only ever reaches an `OpsCtx.namespace`, which is used for *single-object*
/// operations the read API does not perform — every list here is either
/// explicitly namespaced or [`Scope::All`]. It exists so the field is never a
/// lie rather than because anything reads it.
const DEFAULT_NAMESPACE: &str = "default";

/// The whole read API, relative to its mount point.
///
/// Task 8 mounts this under `/api/v1` behind the identity middleware; nothing
/// here is reachable without an [`Identity`] in the request extensions.
pub fn router() -> Router<AppState> {
    Router::new()
        .merge(me::router())
        .merge(graph::router())
        .merge(status::router())
        .merge(repositories::router())
        .merge(snapshots::router())
        .merge(policies::router())
        .merge(schedules::router())
        .merge(restores::router())
        .merge(maintenance::router())
        .merge(replications::router())
        .merge(doctor::router())
        .merge(gates::router())
        .merge(events::router())
}

/// The `?namespace=` every list endpoint accepts.
///
/// Absent means cluster-wide, which is not the same as "the operator's
/// namespace": a fleet view that silently narrowed to one namespace would report
/// a healthy cluster while another namespace burned.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceQuery {
    /// Restrict the listing to this namespace; absent lists cluster-wide.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// Which repository CRD a path or query names.
///
/// A closed enum rather than a bare string so a handler cannot forget the
/// cluster-scoped case, and kebab-case on the wire because it is a URL path
/// segment.
///
/// # One vocabulary for every route that takes this segment
///
/// `GET /repositories/{kind}/{name}` and
/// `DELETE /repositories/{kind}/{name}/session` are the *same* segment in the
/// same URL shape, and they used to parse it two different ways: the read route
/// took kebab-case `cluster-repository` case-**sensitively**, the browse route
/// took `clusterrepository` case-insensitively and rejected the hyphen. The
/// segment that linked one screen was a 400 on the other, from the same row.
///
/// So there is one parser — [`RepositoryKindPath::parse`] — accepting every
/// spelling in [`REPOSITORY_KIND_PATHS`] in any case. What the API *produces* is
/// always the canonical [`RepositoryKindPath::as_path`] (`repository` /
/// `cluster-repository`), which is what `RepositorySummary.kindPath` hands the
/// SPA so it never builds the segment from `kind` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryKindPath {
    /// The namespaced `Repository` CRD.
    Repository,
    /// The cluster-scoped `ClusterRepository` CRD.
    ClusterRepository,
}

/// Every accepted spelling of the `{kind}` path segment, lower-cased.
///
/// The FIRST entry for a kind is its canonical segment — the one
/// [`RepositoryKindPath::as_path`] returns and the one every link the API mints
/// carries. The rest are tolerated because a caller round-tripping
/// `RepositorySummary.kind` (`ClusterRepository`) or hand-writing the CRD kind
/// should not earn a 400 over a hyphen.
pub const REPOSITORY_KIND_PATHS: &[(&str, RepositoryKindPath)] = &[
    ("repository", RepositoryKindPath::Repository),
    ("cluster-repository", RepositoryKindPath::ClusterRepository),
    ("clusterrepository", RepositoryKindPath::ClusterRepository),
];

impl RepositoryKindPath {
    /// The `kopiur_api` kind this path segment names. Exhaustive.
    pub fn kind(self) -> RepositoryKind {
        match self {
            Self::Repository => RepositoryKind::Repository,
            Self::ClusterRepository => RepositoryKind::ClusterRepository,
        }
    }

    /// The path segment for a `kopiur_api` kind. Exhaustive, and the inverse of
    /// [`Self::kind`], so a third repository CRD cannot compile until both
    /// directions name it.
    pub fn from_kind(kind: RepositoryKind) -> Self {
        match kind {
            RepositoryKind::Repository => Self::Repository,
            RepositoryKind::ClusterRepository => Self::ClusterRepository,
        }
    }

    /// The canonical path segment for this kind. Exhaustive.
    pub fn as_path(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::ClusterRepository => "cluster-repository",
        }
    }

    /// Parse one `{kind}` path segment, case-insensitively over every spelling
    /// in [`REPOSITORY_KIND_PATHS`]. `None` for anything else — never a default,
    /// because guessing `Repository` for an unreadable segment would answer
    /// about a namespaced object when the caller named the cluster-scoped one.
    pub fn parse(segment: &str) -> Option<Self> {
        let want = segment.trim().to_ascii_lowercase();
        REPOSITORY_KIND_PATHS
            .iter()
            .find(|(spelling, _)| *spelling == want)
            .map(|(_, kind)| *kind)
    }

    /// The accepted spellings, comma-separated, for a refusal. One list, so a
    /// spelling the parser accepts cannot be left out of the error that names
    /// them.
    pub fn accepted_spellings() -> String {
        REPOSITORY_KIND_PATHS
            .iter()
            .map(|(spelling, _)| *spelling)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Hand-written rather than derived, so `{kind}` is read by [`parse`] — the one
/// parser both routes share — instead of by serde's kebab-case rename, which is
/// exact-match and case-sensitive.
///
/// [`parse`]: RepositoryKindPath::parse
impl<'de> Deserialize<'de> for RepositoryKindPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "`{raw}` is not a repository kind; expected one of {}",
                Self::accepted_spellings()
            ))
        })
    }
}

/// Build the impersonating client for `id`.
///
/// The two ways this fails are already classified by
/// [`ClientBuildError`](crate::auth::impersonate::ClientBuildError) — an identity
/// that cannot be expressed as impersonation headers, and a `kube::Config` the
/// UI itself cannot turn into a client — and `problem.rs` maps both, so this is
/// a `?`-shaped wrapper rather than a second opinion about what went wrong.
///
/// # Errors
///
/// [`ApiError`] when the client cannot be built.
pub fn client_for(app: &AppState, id: &Identity) -> Result<kube::Client, ApiError> {
    Ok(app.auth.clients.client_for(id)?)
}

/// The per-request [`OpsCtx`] the `kopiur_ops` helpers are threaded through.
///
/// `client` impersonates the caller, so anything `kopiur_ops` does with this
/// context is authorized against the human rather than against the UI. The
/// namespace is the request's when it named one, else the operator's, else
/// [`DEFAULT_NAMESPACE`]; the scope follows the same choice, so a `?namespace=`
/// narrows lists and its absence means the whole fleet.
pub fn ops_ctx(cfg: &UiConfig, client: kube::Client, namespace: Option<&str>) -> OpsCtx {
    let scope = match namespace {
        Some(ns) => Scope::Namespace(ns.to_string()),
        None => Scope::All,
    };
    let namespace = namespace
        .map(str::to_string)
        .or_else(|| cfg.operator_namespace.clone())
        .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string());
    OpsCtx {
        client,
        namespace,
        scope,
        field_manager: FIELD_MANAGER.to_string(),
    }
}

/// A `Query<T>` whose rejection is an [`ApiError`].
///
/// axum's own `Query` rejects a malformed or incomplete query string with a
/// `text/plain` 400, *before* any handler runs — so the carefully-worded problem
/// a handler would have returned is unreachable for exactly the case it was
/// written for. `GET /api/v1/events?namespace=media&name=x` with no `kind` never
/// reached `events::incomplete_query`; it got two words of plain text.
///
/// That matters more here than it usually would: `crate::app` tells the SPA it
/// may switch on the content type to decide whether a body is an error, so a
/// `text/plain` 400 is a body the client cannot classify at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct UiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for UiQuery<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(problem(
                400,
                "invalid-query",
                "The query string on this request could not be read.",
                // axum's text names the offending parameter and what it wanted,
                // which is the whole diagnostic value here; wrapping it rather
                // than replacing it keeps that.
                format!("{rejection}"),
                "check the query parameters against the endpoint's documented ones — a required \
                 one is missing, or one carries a value of the wrong type",
            )
            .with_instance(request_path(&parts.extensions, &parts.uri))),
        }
    }
}

/// A `Path<T>` whose rejection is an [`ApiError`]. See [`UiQuery`].
///
/// The case that made this necessary: `/repositories/{kind}` takes the kebab
/// `cluster-repository`, but `RepositorySummary.kind` hands the SPA
/// `ClusterRepository`. A client that round-trips the field it was given gets a
/// path rejection, and it got one as `text/plain`. (The segment itself is now
/// parsed tolerantly — see [`RepositoryKindPath`] — so that particular
/// round-trip resolves; a genuinely unknown kind still lands here.)
#[derive(Debug, Clone, Copy, Default)]
pub struct UiPath<T>(pub T);

impl<S, T> FromRequestParts<S> for UiPath<T>
where
    T: serde::de::DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Path::<T>::from_request_parts(parts, state).await {
            Ok(Path(value)) => Ok(Self(value)),
            Err(rejection) => Err(problem(
                400,
                "invalid-path",
                "A path segment of this request could not be read.",
                format!("{rejection}"),
                format!(
                    "check the URL against the endpoint's shape — the repository kind segment, \
                     for example, is one of {} (in any case)",
                    RepositoryKindPath::accepted_spellings()
                ),
            )
            .with_instance(request_path(&parts.extensions, &parts.uri))),
        }
    }
}

/// **Pure.** Project `Repository`/`ClusterRepository` `status.phase` onto the
/// wire enum. Exhaustive; `Unknown` carries the operator's string verbatim.
pub fn repo_phase_view(p: &RepositoryPhase) -> RepositoryPhaseView {
    match p {
        RepositoryPhase::Pending => RepositoryPhaseView::Pending,
        RepositoryPhase::Initializing => RepositoryPhaseView::Initializing,
        RepositoryPhase::Ready => RepositoryPhaseView::Ready,
        RepositoryPhase::Degraded => RepositoryPhaseView::Degraded,
        RepositoryPhase::Failed => RepositoryPhaseView::Failed,
        RepositoryPhase::Unknown(raw) => RepositoryPhaseView::Unknown { raw: raw.clone() },
    }
}

/// **Pure.** Project `Snapshot` `status.phase` onto the wire enum. Exhaustive.
pub fn snapshot_phase_view(p: &SnapshotPhase) -> SnapshotPhaseView {
    match p {
        SnapshotPhase::Pending => SnapshotPhaseView::Pending,
        SnapshotPhase::Running => SnapshotPhaseView::Running,
        SnapshotPhase::Succeeded => SnapshotPhaseView::Succeeded,
        SnapshotPhase::Failed => SnapshotPhaseView::Failed,
        SnapshotPhase::Deleting => SnapshotPhaseView::Deleting,
        SnapshotPhase::Discovered => SnapshotPhaseView::Discovered,
        SnapshotPhase::Unchanged => SnapshotPhaseView::Unchanged,
        SnapshotPhase::Unknown(raw) => SnapshotPhaseView::Unknown { raw: raw.clone() },
    }
}

/// **Pure.** Project `Restore` `status.phase` onto the wire enum. Exhaustive.
pub fn restore_phase_view(p: &RestorePhase) -> RestorePhaseView {
    match p {
        RestorePhase::Pending => RestorePhaseView::Pending,
        RestorePhase::Resolving => RestorePhaseView::Resolving,
        RestorePhase::Restoring => RestorePhaseView::Restoring,
        RestorePhase::Completed => RestorePhaseView::Completed,
        RestorePhase::Failed => RestorePhaseView::Failed,
        RestorePhase::Unknown(raw) => RestorePhaseView::Unknown { raw: raw.clone() },
    }
}

/// **Pure.** Project `RepositoryReplication` `status.phase` onto the wire enum.
/// Exhaustive.
///
/// The two replication kinds have *separate* phase enums in `kopiur_api` —
/// identical today, free to diverge — so this projection is written twice rather
/// than made generic. A shared function would need a trait whose only purpose is
/// to erase the distinction the API deliberately keeps.
pub fn repository_replication_phase_view(p: &RepositoryReplicationPhase) -> ReplicationPhaseView {
    match p {
        RepositoryReplicationPhase::Pending => ReplicationPhaseView::Pending,
        RepositoryReplicationPhase::Replicating => ReplicationPhaseView::Replicating,
        RepositoryReplicationPhase::Succeeded => ReplicationPhaseView::Succeeded,
        RepositoryReplicationPhase::Failed => ReplicationPhaseView::Failed,
        RepositoryReplicationPhase::Suspended => ReplicationPhaseView::Suspended,
        RepositoryReplicationPhase::Unknown(raw) => {
            ReplicationPhaseView::Unknown { raw: raw.clone() }
        }
    }
}

/// **Pure.** Project `SnapshotReplication` `status.phase` onto the wire enum.
/// Exhaustive. See [`repository_replication_phase_view`] for why there are two.
pub fn snapshot_replication_phase_view(p: &SnapshotReplicationPhase) -> ReplicationPhaseView {
    match p {
        SnapshotReplicationPhase::Pending => ReplicationPhaseView::Pending,
        SnapshotReplicationPhase::Replicating => ReplicationPhaseView::Replicating,
        SnapshotReplicationPhase::Succeeded => ReplicationPhaseView::Succeeded,
        SnapshotReplicationPhase::Failed => ReplicationPhaseView::Failed,
        SnapshotReplicationPhase::Suspended => ReplicationPhaseView::Suspended,
        SnapshotReplicationPhase::Unknown(raw) => {
            ReplicationPhaseView::Unknown { raw: raw.clone() }
        }
    }
}

/// **Pure.** The display string for a `Maintenance` manual-run phase.
///
/// `ManualRunView.phase` is a `String` on the wire rather than a view enum, so
/// the projection is [`kopiur_api::common::PhaseLabel::label`] — the same string
/// the operator wrote, `Unknown` included.
pub fn manual_run_phase_label(p: &ManualRunPhase) -> String {
    use kopiur_api::common::PhaseLabel as _;
    p.label().to_string()
}

/// **Pure.** The display string for one populator claim's phase. As with
/// [`manual_run_phase_label`], the wire field is a `String`.
pub fn restore_claim_phase_label(p: &RestoreClaimPhase) -> String {
    use kopiur_api::common::PhaseLabel as _;
    p.label().to_string()
}

/// **Pure.** Project `Snapshot` `status.origin` onto the wire enum. Exhaustive.
///
/// [`Origin`] has no `Unknown` variant — it is parsed strictly and an
/// unrecognized marker never decodes — so this mapping is total.
pub fn origin_view(o: Origin) -> OriginView {
    match o {
        Origin::Scheduled => OriginView::Scheduled,
        Origin::Manual => OriginView::Manual,
        Origin::Discovered => OriginView::Discovered,
        Origin::Adopted => OriginView::Adopted,
        Origin::Replicated => OriginView::Replicated,
    }
}

/// **Pure.** Flatten one `status.conditions[]` entry for display.
pub fn condition_view(c: &Condition) -> ConditionView {
    ConditionView {
        r#type: c.type_.clone(),
        status: c.status.clone(),
        reason: non_empty(&c.reason),
        message: non_empty(&c.message),
        last_transition_time: rfc3339(&c.last_transition_time),
    }
}

/// **Pure.** [`condition_view`] over a whole condition list.
pub fn conditions_view(conditions: &[Condition]) -> Vec<ConditionView> {
    conditions.iter().map(condition_view).collect()
}

/// `None` for an empty string, so an unset `reason`/`message` is omitted from
/// the JSON rather than rendered as an empty chip.
fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// **Pure.** A Kubernetes `Time` as RFC 3339, or `None` when it cannot be
/// represented.
pub fn rfc3339(t: &Time) -> Option<String> {
    kopiur_ops::snapshots::meta_time(t).map(|t| t.to_rfc3339())
}

/// **Pure.** The stable display key for a repository reference —
/// `Repository/<namespace>/<name>` or `ClusterRepository/<name>`.
///
/// This is [`repo_key`], not a second format: it is what the graph uses as a
/// node id and what every "same repository?" comparison in this crate compares,
/// so the string a user sees and the string the code joins on are one value.
/// `owner_ns` is the namespace an absent `ref.namespace` resolves against — the
/// referring object's own.
pub fn repo_ref_display(r: &RepositoryRef, owner_ns: Option<&str>) -> String {
    repo_key(r, owner_ns.unwrap_or_default())
}

/// **Pure.** The wire severity for one registry row.
///
/// THE projection from `kopiur_api`'s two-level severity onto the wire's, and
/// the only one: `gate_hits` and [`gates::descriptor`](crate::api::gates::descriptor)
/// both call it, so the severity a gate is *documented* with and the severity a
/// live hit *reports* cannot drift apart. They did — one shipped `Fail` and the
/// other `error` for the same registry row — which is what makes this a function
/// rather than two `match`es. Exhaustive.
pub fn gate_severity_view(severity: GateSeverity) -> GateSeverityView {
    match severity {
        GateSeverity::Fail => GateSeverityView::Error,
        GateSeverity::Warn => GateSeverityView::Warning,
    }
}

/// **Pure.** Every `Maintenance` that governs this repository — the namespace
/// guard included.
///
/// [`kopiur_ops::maintenance::covers_repository`] takes no namespace and
/// resolves an absent ref namespace against the **`Maintenance`'s own**
/// namespace. So a `Maintenance` in namespace `backup` pointing at
/// `Repository nas` answers `true` when asked about `Repository/media/nas`, and
/// same-named repositories across namespaces are the normal case in this
/// operator. A caller that forgets the guard renders one namespace's repository
/// with another namespace's maintenance state.
///
/// One helper rather than a rule written twice: the repository detail screen had
/// the guard and the fleet graph did not, so a failing compaction in `backup`
/// coloured `media`'s repository degraded.
pub fn covering_maintenances<'a>(
    maintenances: &'a [std::sync::Arc<kopiur_api::Maintenance>],
    kind: RepositoryKind,
    name: &'a str,
    namespace: Option<&'a str>,
) -> impl Iterator<Item = &'a std::sync::Arc<kopiur_api::Maintenance>> {
    maintenances
        .iter()
        .filter(move |m| match kind {
            // Same-namespace semantics, matching what `covers_repository`
            // assumes about the slice it is handed.
            RepositoryKind::Repository => m.metadata.namespace.as_deref() == namespace,
            RepositoryKind::ClusterRepository => true,
        })
        .filter(move |m| kopiur_ops::maintenance::covers_repository(m, kind, name))
}

/// **Pure.** Which structural gates a resource's live conditions have tripped.
///
/// `covers` is the scope predicate for the kind being inspected — one of
/// [`GateScope`]'s `covers_*` methods — so a `Snapshot`'s conditions are never
/// matched against a `Repository`-only gate.
///
/// # One hit per condition
///
/// A live condition is identified by [`StructuralGate::matches`]
/// (type + status + reason), which selects exactly one registry row. Filtering
/// on [`StructuralGate::trips`] alone would double-report every condition whose
/// `type` has several registered reasons. But `matches` alone would go *silent*
/// on a reason a newer operator invented, which is the failure #359 was, so a
/// condition that trips a gate's polarity with an unregistered reason still
/// yields a hit — carrying the operator's own reason string and the tripping
/// row's severity.
///
/// [`StructuralGate::matches`]: kopiur_api::gates::StructuralGate::matches
/// [`StructuralGate::trips`]: kopiur_api::gates::StructuralGate::trips
pub fn gate_hits(conditions: &[Condition], covers: fn(GateScope) -> bool) -> Vec<GateHit> {
    conditions
        .iter()
        .filter_map(|c| {
            let exact = STRUCTURAL_GATES
                .iter()
                .find(|g| covers(g.applies_to) && g.matches(&c.type_, &c.status, &c.reason));
            let row = match exact {
                Some(row) => row,
                None => STRUCTURAL_GATES
                    .iter()
                    .find(|g| covers(g.applies_to) && g.trips(&c.type_, &c.status))?,
            };
            Some(GateHit {
                condition: c.type_.clone(),
                reason: c.reason.clone(),
                severity: gate_severity_view(row.severity),
                message: c.message.clone(),
            })
        })
        .collect()
}

/// **Pure.** Cut `items` down to the `offset`/`limit` window, keeping the totals
/// the SPA needs to render "showing 21-40 of 137".
///
/// `offset` past the end yields an empty page rather than an error: a user who
/// deep-links page 9 of a list that has since shrunk should see an empty table,
/// not a 400.
pub fn paginate<T>(items: Vec<T>, offset: usize, limit: usize) -> Page<T> {
    let total = items.len();
    let page = items.into_iter().skip(offset).take(limit).collect();
    Page {
        items: page,
        total,
        offset,
        limit,
    }
}

/// The 422 a listing answers when the caller's filter selected more rows than
/// the deployment will assemble.
///
/// A cap rather than a silent truncation: a snapshot table that quietly dropped
/// rows would be read as "these are all my backups".
pub fn list_too_large(kind: &str, matched: usize, cap: usize) -> ApiError {
    problem(
        422,
        "list-too-large",
        format!(
            "{matched} {kind} matched, which is more than this kopiur-ui will assemble in one response (the cap is {cap})."
        ),
        "Rendering an unbounded list would hold the whole result set in memory on the server and in the browser, so kopiur-ui refuses instead of degrading for everyone on the page.",
        "filter by repository or policy — or raise KOPIUR_UI_SNAPSHOT_LIST_CAP if this cluster really does need a list this long",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::gates::GateScope;

    /// A condition, spelled as compactly as the fixtures need it.
    fn cond(type_: &str, status: &str, reason: &str, message: &str) -> Condition {
        Condition {
            type_: type_.to_string(),
            status: status.to_string(),
            reason: reason.to_string(),
            message: message.to_string(),
            observed_generation: None,
            // Built by deserialization rather than from a `jiff::Timestamp`
            // literal: `Time`'s inner type is k8s-openapi's choice, not this
            // crate's dependency.
            last_transition_time: serde_json::from_value(serde_json::json!("2026-09-08T10:00:00Z"))
                .expect("an RFC3339 instant parses as a Time"),
        }
    }

    #[test]
    fn every_repository_phase_has_a_view_including_unknown() {
        use kopiur_api::common::PhaseLabel as _;
        for phase in RepositoryPhase::ALL {
            // A canonical phase never renders as the fallback chip.
            assert!(
                !matches!(repo_phase_view(phase), RepositoryPhaseView::Unknown { .. }),
                "{} must have a canonical view",
                phase.label()
            );
        }
        assert_eq!(
            repo_phase_view(&RepositoryPhase::Unknown("Weird".into())),
            RepositoryPhaseView::Unknown {
                raw: "Weird".into()
            },
            "an unrecognized phase carries the operator's string verbatim"
        );
        assert_eq!(
            repo_phase_view(&RepositoryPhase::Ready),
            RepositoryPhaseView::Ready
        );
        assert_eq!(
            repo_phase_view(&RepositoryPhase::Initializing),
            RepositoryPhaseView::Initializing
        );
    }

    #[test]
    fn every_snapshot_phase_has_a_view_including_unchanged_and_unknown() {
        use kopiur_api::common::PhaseLabel as _;
        for phase in SnapshotPhase::ALL {
            assert!(
                !matches!(
                    snapshot_phase_view(phase),
                    SnapshotPhaseView::Unknown { .. }
                ),
                "{} must have a canonical view",
                phase.label()
            );
        }
        // #351's variant, the one two `_ =>` arms swallowed: it is a distinct
        // chip, not folded into Succeeded.
        assert_eq!(
            snapshot_phase_view(&SnapshotPhase::Unchanged),
            SnapshotPhaseView::Unchanged
        );
        assert_eq!(
            snapshot_phase_view(&SnapshotPhase::Unknown("Quiescing".into())),
            SnapshotPhaseView::Unknown {
                raw: "Quiescing".into()
            }
        );
    }

    #[test]
    fn every_restore_and_replication_phase_has_a_view() {
        use kopiur_api::common::PhaseLabel as _;
        for phase in RestorePhase::ALL {
            assert!(
                !matches!(restore_phase_view(phase), RestorePhaseView::Unknown { .. }),
                "{} must have a canonical view",
                phase.label()
            );
        }
        for phase in RepositoryReplicationPhase::ALL {
            assert!(
                !matches!(
                    repository_replication_phase_view(phase),
                    ReplicationPhaseView::Unknown { .. }
                ),
                "{} must have a canonical view",
                phase.label()
            );
        }
        for phase in SnapshotReplicationPhase::ALL {
            assert!(
                !matches!(
                    snapshot_replication_phase_view(phase),
                    ReplicationPhaseView::Unknown { .. }
                ),
                "{} must have a canonical view",
                phase.label()
            );
        }
        assert_eq!(
            restore_phase_view(&RestorePhase::Unknown("Staging".into())),
            RestorePhaseView::Unknown {
                raw: "Staging".into()
            }
        );
        // The two replication enums are distinct types that must project onto
        // the ONE wire enum, so a divergence would be a compile error here.
        assert_eq!(
            repository_replication_phase_view(&RepositoryReplicationPhase::Suspended),
            snapshot_replication_phase_view(&SnapshotReplicationPhase::Suspended)
        );
    }

    #[test]
    fn every_origin_has_a_view() {
        let views: Vec<OriginView> = Origin::ALL.iter().map(|o| origin_view(*o)).collect();
        assert_eq!(views.len(), 5, "a new origin needs a wire view");
        assert_eq!(origin_view(Origin::Replicated), OriginView::Replicated);
        assert_eq!(origin_view(Origin::Adopted), OriginView::Adopted);
    }

    #[test]
    fn a_condition_view_omits_empty_reason_and_message() {
        let full = condition_view(&cond("Ready", "False", "NotConnected", "cannot reach s3"));
        assert_eq!(full.r#type, "Ready");
        assert_eq!(full.status, "False");
        assert_eq!(full.reason.as_deref(), Some("NotConnected"));
        assert_eq!(full.message.as_deref(), Some("cannot reach s3"));
        assert!(
            full.last_transition_time.is_some(),
            "a transition time always renders"
        );

        let bare = condition_view(&cond("Ready", "True", "", ""));
        assert_eq!(bare.reason, None, "an empty reason is absent, not empty");
        assert_eq!(bare.message, None);
    }

    #[test]
    fn repo_ref_display_is_the_repo_key() {
        let namespaced = RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "nas".into(),
            namespace: None,
        };
        assert_eq!(
            repo_ref_display(&namespaced, Some("media")),
            "Repository/media/nas",
            "an absent ref namespace resolves against the owner's"
        );
        let explicit = RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "nas".into(),
            namespace: Some("storage".into()),
        };
        assert_eq!(
            repo_ref_display(&explicit, Some("media")),
            "Repository/storage/nas"
        );
        let cluster = RepositoryRef {
            kind: RepositoryKind::ClusterRepository,
            name: "shared".into(),
            namespace: None,
        };
        assert_eq!(
            repo_ref_display(&cluster, Some("media")),
            "ClusterRepository/shared",
            "a cluster repository is namespace-free"
        );
    }

    #[test]
    fn gate_hits_reports_one_row_per_tripped_condition() {
        let conditions = vec![
            cond("Ready", "True", "Connected", "connected"),
            cond(
                "MassDeletionHeld",
                "True",
                "ThresholdExceeded",
                "42 deletions exceed the threshold of 10",
            ),
        ];
        let hits = gate_hits(&conditions, GateScope::covers_repository);
        assert_eq!(hits.len(), 1, "only the blocked condition is a hit");
        assert_eq!(hits[0].condition, "MassDeletionHeld");
        assert_eq!(hits[0].reason, "ThresholdExceeded");
        assert_eq!(hits[0].message, "42 deletions exceed the threshold of 10");
        // The mass-deletion breaker is a hard block, so the SPA must be told to
        // style it as an error. The previous version of this assertion accepted
        // either of the registry's OWN labels ("Fail"/"Warn"), which is how a hit
        // shipping a vocabulary the wire type forbids passed review green.
        assert_eq!(hits[0].severity, GateSeverityView::Error);
    }

    #[test]
    fn gate_hits_still_fires_for_a_reason_this_build_never_registered() {
        // The #359 shape one version on: the operator kept the gate's condition
        // and polarity but invented a new reason. Going silent here is the bug.
        let conditions = vec![cond(
            "MassDeletionHeld",
            "True",
            "BudgetExhausted",
            "a reason from a newer operator",
        )];
        let hits = gate_hits(&conditions, GateScope::covers_repository);
        assert_eq!(
            hits.len(),
            1,
            "an unregistered reason must not go unreported"
        );
        assert_eq!(
            hits[0].reason, "BudgetExhausted",
            "the operator's own reason is surfaced, not the registry's"
        );
    }

    #[test]
    fn gate_hits_respects_scope() {
        // `MoverPermitted=False` is a Snapshot/Restore gate; a Repository must
        // not report it even if the condition somehow appears on one.
        let conditions = vec![cond(
            "MoverPermitted",
            "False",
            "PrivilegedMoverNotPermitted",
            "namespace has not opted in",
        )];
        assert!(gate_hits(&conditions, GateScope::covers_repository).is_empty());
        assert_eq!(gate_hits(&conditions, GateScope::covers_snapshot).len(), 1);
    }

    #[test]
    fn paginate_windows_and_keeps_the_total() {
        let page = paginate((0..10).collect::<Vec<i32>>(), 4, 3);
        assert_eq!(page.items, vec![4, 5, 6]);
        assert_eq!(page.total, 10);
        assert_eq!(page.offset, 4);
        assert_eq!(page.limit, 3);
    }

    #[test]
    fn paginate_past_the_end_is_an_empty_page_not_an_error() {
        let page = paginate((0..3).collect::<Vec<i32>>(), 100, 20);
        assert!(page.items.is_empty());
        assert_eq!(
            page.total, 3,
            "the total still describes the whole result set"
        );
    }

    #[test]
    fn list_too_large_names_the_cap_and_a_remedy() {
        let err = list_too_large("snapshots", 9001, 5000);
        assert_eq!(err.0.status, 422);
        assert_eq!(err.0.r#type, "urn:kopiur:problem:list-too-large");
        assert!(
            err.0.what.contains("9001"),
            "the problem states what matched"
        );
        assert!(err.0.what.contains("5000"), "and the cap it exceeded");
        assert!(
            err.0.fix.contains("repository") && err.0.fix.contains("policy"),
            "the remedy names the filters that would work: {}",
            err.0.fix
        );
    }

    #[test]
    fn the_whole_read_api_composes_into_one_router() {
        // `Router::merge` panics on a path two modules both claim, and a route
        // that never composes is a route Task 8 mounts and nobody can call. The
        // construction IS the assertion; there is nothing to compare it against
        // that would not just restate the thirteen `route(..)` calls.
        let _router: Router<AppState> = router();
    }

    /// The whole point of item 3: every spelling a caller could plausibly send —
    /// the canonical kebab segment, the CRD kind `RepositorySummary.kind` hands
    /// them, and the hyphen-less form the browse route used to insist on —
    /// resolves to the same kind, through one parser.
    #[test]
    fn every_accepted_repository_kind_spelling_resolves_to_the_same_kind() {
        for spelling in [
            "cluster-repository",
            "clusterrepository",
            "ClusterRepository",
            "Cluster-Repository",
            "CLUSTERREPOSITORY",
        ] {
            assert_eq!(
                RepositoryKindPath::parse(spelling).map(RepositoryKindPath::kind),
                Some(RepositoryKind::ClusterRepository),
                "{spelling} must name the cluster-scoped CRD"
            );
        }
        for spelling in ["repository", "Repository", "REPOSITORY"] {
            assert_eq!(
                RepositoryKindPath::parse(spelling).map(RepositoryKindPath::kind),
                Some(RepositoryKind::Repository),
                "{spelling} must name the namespaced CRD"
            );
        }
        assert_eq!(
            RepositoryKindPath::parse("secret"),
            None,
            "an unknown segment is a refusal, never a default"
        );
    }

    /// The extractor reads a query/path value through the same parser, so a
    /// `?repositoryKind=ClusterRepository` is not a 400 the way it used to be.
    #[test]
    fn the_deserializer_is_the_shared_parser_and_its_error_names_every_spelling() {
        let cluster: RepositoryKindPath =
            serde_json::from_value(serde_json::json!("ClusterRepository")).unwrap();
        assert_eq!(cluster.kind(), RepositoryKind::ClusterRepository);
        let error = serde_json::from_value::<RepositoryKindPath>(serde_json::json!("Secret"))
            .expect_err("not a repository kind");
        for spelling in ["repository", "cluster-repository", "clusterrepository"] {
            assert!(
                error.to_string().contains(spelling),
                "the refusal must list {spelling}: {error}"
            );
        }
    }

    /// The canonical segment is what the API *mints*, and it round-trips.
    #[test]
    fn the_canonical_path_segment_round_trips_through_the_parser() {
        for kind in [
            RepositoryKind::Repository,
            RepositoryKind::ClusterRepository,
        ] {
            let path = RepositoryKindPath::from_kind(kind);
            assert_eq!(path.kind(), kind, "from_kind and kind must be inverses");
            assert_eq!(
                RepositoryKindPath::parse(path.as_path()),
                Some(path),
                "the segment the API hands the SPA must be one the API accepts"
            );
        }
        assert_eq!(
            RepositoryKindPath::from_kind(RepositoryKind::ClusterRepository).as_path(),
            "cluster-repository",
            "kebab is canonical; the other spellings are tolerated, not produced"
        );
    }
}

/// Router-level tests that a rejection *before any handler runs* is still a
/// problem document.
///
/// These build the real stack — the identity middleware in front of
/// [`router`] — because that is the only way the ordering is honest: extractors
/// run in declaration order, so `CurrentIdentity` resolves first and a test
/// without the middleware would 500 on identity and never reach the query.
#[cfg(test)]
mod extractor_rejection_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header::CONTENT_TYPE};
    use http_body_util::BodyExt as _;
    use std::sync::Arc;
    use tower::ServiceExt as _;

    fn anonymous_auth_config() -> crate::config::AuthConfig {
        use crate::config::*;
        AuthConfig {
            mode: AuthMode::AnonymousOnly(AnonymousIdentity {
                user: "viewer".to_string(),
                groups: Vec::new(),
            }),
            groups_separator: DEFAULT_GROUPS_SEPARATOR.to_string(),
            email_header: None,
            extra_keys: Vec::new(),
            allowed_groups: None,
            proxy_secret: None,
        }
    }

    fn test_config() -> UiConfig {
        use crate::config::*;
        UiConfig {
            addr: DEFAULT_ADDR.parse().unwrap(),
            ops_addr: DEFAULT_OPS_ADDR.parse().unwrap(),
            auth: anonymous_auth_config(),
            operator_namespace: None,
            mover_image: None,
            cache_enabled: false,
            session: SessionLimits {
                ttl: std::time::Duration::from_secs(900),
                ready_timeout: std::time::Duration::from_secs(300),
                max_starts: DEFAULT_MAX_SESSION_STARTS,
                max_exec_per_identity: DEFAULT_MAX_EXEC_PER_IDENTITY,
                max_exec_global: DEFAULT_MAX_EXEC_GLOBAL,
            },
            download_max_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
            manifest_max_bytes: DEFAULT_MAX_MANIFEST_BYTES,
            download_chunk_timeout: std::time::Duration::from_secs(60),
            snapshot_list_cap: DEFAULT_SNAPSHOT_LIST_CAP,
            client_cache: CacheLimits {
                size: DEFAULT_CLIENT_CACHE_SIZE,
                ttl: std::time::Duration::from_secs(600),
            },
            sar_ttl: std::time::Duration::from_secs(60),
            sar_cache_size: DEFAULT_SAR_CACHE_SIZE,
            tls: None,
            cors_origins: Vec::new(),
        }
    }

    /// An `AppState` whose auth is WIRED — `AuthState::unconfigured()` fails
    /// closed and 500s every request, which would mask the 400 under test — and
    /// whose anonymous identity resolves with no headers at all.
    fn wired_state() -> AppState {
        let provider = Arc::new(kopiur_telemetry::MetricsProvider::new("kopiur-ui-test"));
        AppState {
            cfg: Arc::new(test_config()),
            metrics: Arc::new(crate::metrics::UiMetrics::new(provider)),
            readiness: Arc::new(crate::ops_listener::Readiness::new(
                crate::static_files::is_placeholder(),
            )),
            auth: Arc::new(crate::auth::AuthState::new(
                anonymous_auth_config(),
                // Points at nothing: these requests must fail at the extractor,
                // long before anything would dial a cluster.
                kube::Config::new("http://127.0.0.1:1/".parse().expect("a literal URL")),
                crate::config::CacheLimits {
                    size: 1,
                    ttl: std::time::Duration::from_secs(60),
                },
            )),
            source: Arc::new(crate::cache::Source::Impersonated),
            sessions: Arc::new(crate::browse::session_pool::SessionPool::default()),
        }
    }

    /// `GET uri` through the identity middleware and the read API.
    async fn get(uri: &str) -> (StatusCode, Option<String>, serde_json::Value) {
        let state = wired_state();
        let app = router()
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::auth::identity_middleware,
            ))
            .with_state(state);
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, content_type, body)
    }

    #[tokio::test]
    async fn a_missing_required_query_parameter_is_a_problem_document() {
        // `/events` needs all three; omitting `kind` is an axum QueryRejection,
        // which used to escape as two words of `text/plain`.
        let (status, content_type, body) = get("/events?namespace=media&name=nightly-1").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            content_type.as_deref(),
            Some("application/problem+json"),
            "the SPA switches on the content type to decide whether a body is an error"
        );
        assert_eq!(body["type"], "urn:kopiur:problem:invalid-query");
        assert_eq!(body["status"], 400);
        assert!(
            body["why"].as_str().is_some_and(|w| w.contains("kind")),
            "the rejection names the parameter it wanted: {}",
            body["why"]
        );
        assert!(!body["fix"].as_str().unwrap_or_default().is_empty());
        assert_eq!(body["instance"], "/events");
    }

    #[tokio::test]
    async fn a_bad_path_segment_is_a_problem_document() {
        // A segment that is not a repository kind at all. (The kind spellings a
        // client could plausibly round-trip — `Repository`,
        // `ClusterRepository`, `clusterrepository` — all resolve now; see
        // `every_accepted_repository_kind_spelling_resolves_to_the_same_kind`.)
        let (status, content_type, body) = get("/repositories/Secret/nas?namespace=media").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(content_type.as_deref(), Some("application/problem+json"));
        assert_eq!(body["type"], "urn:kopiur:problem:invalid-path");
        assert!(
            body["fix"]
                .as_str()
                .is_some_and(|f| f.contains("cluster-repository")),
            "the remedy spells the form that would have worked: {}",
            body["fix"]
        );
    }

    /// The read route and the browse session-delete route take the same segment
    /// out of the same URL shape, so every spelling must resolve on BOTH. A
    /// resolved segment means the request got past routing and path parsing into
    /// the handler — where it fails for want of a cluster, which is neither a
    /// 400 `invalid-path` nor a 404.
    #[tokio::test]
    async fn every_kind_spelling_resolves_on_the_repository_read_route() {
        for spelling in [
            "repository",
            "Repository",
            "cluster-repository",
            "clusterrepository",
            "ClusterRepository",
        ] {
            let (status, _, body) =
                get(&format!("/repositories/{spelling}/nas?namespace=media")).await;
            assert_ne!(
                status,
                StatusCode::NOT_FOUND,
                "{spelling} must match the route"
            );
            assert_ne!(
                body["type"], "urn:kopiur:problem:invalid-path",
                "{spelling} must be a kind this route reads, got {body}"
            );
        }
    }

    #[tokio::test]
    async fn a_non_numeric_pagination_parameter_is_a_problem_document() {
        let (status, content_type, body) = get("/snapshots?limit=lots").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(content_type.as_deref(), Some("application/problem+json"));
        assert_eq!(body["type"], "urn:kopiur:problem:invalid-query");
    }
}
