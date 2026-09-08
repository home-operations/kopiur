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

// Every endpoint returns `Result<Json<T>, ApiError>`, and an `ApiError` is a
// `Problem`: eight owned `String`s of what/why/fix prose, about 200 bytes. That
// trips `clippy::result_large_err`, whose remedy — boxing the error — is not
// available here: axum resolves a handler's error type through `IntoResponse`,
// which is implemented on `ApiError` itself in `problem.rs`, and the size is
// paid once per *failed* request rather than per call in a hot loop. Allowed at
// the module root so every child module inherits one decision instead of
// scattering the same annotation across thirteen files.
#![allow(clippy::result_large_err)]

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
use serde::Deserialize;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};

use kopiur_api::common::{RepositoryKind, RepositoryRef, repo_key};
use kopiur_api::gates::{GateScope, STRUCTURAL_GATES};
use kopiur_api::maintenance::ManualRunPhase;
use kopiur_api::repository_replication::RepositoryReplicationPhase;
use kopiur_api::restore::RestoreClaimPhase;
use kopiur_api::snapshot_replication::SnapshotReplicationPhase;
use kopiur_api::{Origin, RepositoryPhase, RestorePhase, SnapshotPhase};
use kopiur_ops::{OpsCtx, Scope};
use kopiur_ui_model::graph::GateHit;
use kopiur_ui_model::views::{
    ConditionView, OriginView, Page, ReplicationPhaseView, RepositoryPhaseView, RestorePhaseView,
    SnapshotPhaseView,
};

use crate::AppState;
use crate::api::problem::{ApiError, problem};
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RepositoryKindPath {
    /// The namespaced `Repository` CRD.
    Repository,
    /// The cluster-scoped `ClusterRepository` CRD.
    ClusterRepository,
}

impl RepositoryKindPath {
    /// The `kopiur_api` kind this path segment names. Exhaustive.
    pub fn kind(self) -> RepositoryKind {
        match self {
            Self::Repository => RepositoryKind::Repository,
            Self::ClusterRepository => RepositoryKind::ClusterRepository,
        }
    }
}

/// Build the impersonating client for `id`.
///
/// A failure here is never the caller's fault — the client is constructed from
/// the UI's own resolved `kube::Config` — so it is classified through
/// [`kopiur_ops::classify_kube`] rather than guessed at, which keeps the one
/// case that *can* happen in practice (a malformed impersonation header value
/// the config allows) readable in the browser.
///
/// # Errors
///
/// [`ApiError`] when the client cannot be built.
pub fn client_for(app: &AppState, id: &Identity) -> Result<kube::Client, ApiError> {
    match app.auth.clients.client_for(id) {
        Ok(client) => Ok(client),
        Err(e) => Err(ApiError::from(kopiur_ops::classify_kube(
            "build", "Client", "clients", None, None, e,
        ))),
    }
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
                severity: row.severity.label().to_string(),
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
        assert!(
            hits[0].severity == "Fail" || hits[0].severity == "Warn",
            "severity is a registry label, got {}",
            hits[0].severity
        );
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

    #[test]
    fn a_repository_kind_path_segment_is_kebab_case() {
        let cluster: RepositoryKindPath =
            serde_json::from_value(serde_json::json!("cluster-repository")).unwrap();
        assert_eq!(cluster.kind(), RepositoryKind::ClusterRepository);
        let namespaced: RepositoryKindPath =
            serde_json::from_value(serde_json::json!("repository")).unwrap();
        assert_eq!(namespaced.kind(), RepositoryKind::Repository);
        assert!(
            serde_json::from_value::<RepositoryKindPath>(serde_json::json!("Repository")).is_err(),
            "the path segment is the kebab form only"
        );
    }
}
