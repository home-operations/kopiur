//! The mutating endpoints: snapshot-now, restore, suspend/resume, maintenance
//! run, replication run, catalog scan, and snapshot deletion.
//!
//! Each one builds the same request type the CLI does (`kopiur_ops::actions`) and
//! applies it under the caller's impersonated identity with the
//! [`crate::config::FIELD_MANAGER`] field manager, so a change made in the UI is
//! attributable in `managedFields` and identical to the one `kubectl kopiur`
//! would have made.
//!
//! # What this module is allowed to decide
//!
//! Almost nothing. A handler here does exactly four things: resolve the caller's
//! impersonating client, translate one `kopiur_ui_model::requests` body into the
//! `kopiur_ops` request type, call the shared builder plus the shared IO, and
//! turn the result into an [`ActionReceipt`]. Every decision that could differ
//! between the CLI and the UI — how a `Snapshot` is named, which cells a fan-out
//! mints, what a run-request annotation says — lives in `kopiur_ops` and is
//! shared verbatim. The translation below is therefore mechanical, and its tests
//! assert the mechanics rather than the semantics.
//!
//! # A write-consequential read is never served from the reflector
//!
//! Nothing in this module touches [`crate::cache::Source`]. The read side may
//! answer from the watch-fed reflector — a page rendering a policy one watch-lag
//! stale is a cosmetic problem — but a mutation's *input* is not a view. The
//! `SnapshotPolicy` that `snapshot-now` reads is what decides how many `Snapshot`
//! CRs get created and against which repositories, so it is fetched live through
//! the caller's own impersonated client. Staleness there would not misrender a
//! page; it would create the wrong backups. The rule is absolute rather than
//! per-read so it stays greppable: `actions/` contains zero references to
//! `app.source`.
//!
//! # Why the string fields are parsed here and not in the wire types
//!
//! `SuspendBody::kind`, `MaintenanceRunBody::mode` and `ScanCatalogBody::kind`
//! are `String` on the wire, because `kopiur-ui-model` is the SPA's type source
//! and must not depend on `kopiur-ops`. The closed enums they name live in
//! `kopiur_ops`/`kopiur_api`, so the parse happens at the edge — once, in one
//! table per family, with an exhaustive `match` on the way out and a 400 naming
//! every accepted spelling on the way in. A body that names a kind this build
//! does not know is refused; it never falls through to a default.
//!
//! # The receipts are deliberately thin
//!
//! An action's answer is "we asked, here is what was created or when it was
//! requested" — never the finished outcome. Snapshots, restores, maintenance and
//! replication all run as mover Jobs the operator schedules, so the UI reports
//! the accepted request (`201` for the CRs it created, `202` for the annotations
//! it stamped) and lets the SPA watch the object's status for the rest.

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use chrono::Utc;

use kube::api::{Api, DeleteParams};

use kopiur_api::common::{RepositoryKind, RepositoryRef};
use kopiur_api::consts::DELETION_HELD_CONDITION;
use kopiur_api::restore::{FromPolicy, IdentitySource, PvcTemplate, RestoreOptions};
use kopiur_api::{
    ManualRunMode, ObjectRef, RestoreSource, RestoreTarget, Snapshot, SnapshotPolicy,
};
use kopiur_ops::actions::catalog;
use kopiur_ops::actions::restore::{RestoreRequest, build_restore, create_restore};
use kopiur_ops::actions::snapshot::{SnapshotNowRequest, create_snapshots, plan_snapshots};
use kopiur_ops::maintenance::{self, MaintenanceTarget};
use kopiur_ops::replication::{self, ReplicationKind};
use kopiur_ops::suspend::{self, SuspendReport, SuspendableKind};
use kopiur_ops::{OpsCtx, OpsError, Scope, classify_kube, scope_suffix};

use kopiur_ui_model::requests::{
    MaintenanceRunBody, ReplicationRunBody, RepositoryRefBody, RestoreBody, RestoreSourceBody,
    RestoreTargetBody, ScanCatalogBody, SnapshotNowBody, SuspendBody,
};
use kopiur_ui_model::views::{ActionReceipt, SnapshotRefView};

use crate::AppState;
use crate::api::problem::{ApiError, problem, request_path};
use crate::auth::identity::Identity;
use crate::auth::{CurrentIdentity, mutation_guard};
use crate::config::FIELD_MANAGER;

/// Every mutating route, gated by [`mutation_guard`].
///
/// Mounted by [`crate::app`] under `/api/v1`, inside the identity middleware —
/// the guard here is the CSRF half, the middleware outside is the impersonation
/// half, and a route that escaped either would be a wiring bug rather than a
/// degraded mode (`CurrentIdentity` answers 500 rather than guessing a caller).
///
/// The layer is applied after every route is registered, so it covers all of
/// them; a route appended *below* the `.layer(..)` call would silently skip the
/// CSRF check, which is why nothing may follow it.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/actions/snapshot-now", post(snapshot_now))
        .route("/actions/restore", post(restore))
        .route("/actions/suspend", post(set_suspended))
        .route("/actions/maintenance-run", post(maintenance_run))
        .route("/actions/replication-run", post(replication_run))
        .route("/actions/scan-catalog", post(scan_catalog))
        .route("/snapshots/{namespace}/{name}", delete(delete_snapshot))
        .layer(axum::middleware::from_fn(mutation_guard))
}

// --- body extraction --------------------------------------------------------

/// [`axum::Json`], but a malformed body is an `application/problem+json` 400
/// instead of axum's plain-text rejection.
///
/// The point is the message: every request type is
/// `#[serde(deny_unknown_fields)]`, so a mistyped field name is a *rejection*
/// rather than a silently ignored option — and that is only useful if the caller
/// is told which field. serde's own message names it, so it is carried into
/// `what` verbatim rather than paraphrased.
///
/// Two rejections, not one. A body that exceeded axum's length limit is a 413
/// `body-too-large`, because "your request is malformed" is a false diagnosis
/// that sends the caller looking for a typo they do not have. Everything else is
/// the 400 above.
///
/// The remaining variants axum would status differently — `MissingJsonContentType`
/// is a 415 there — collapse into the 400 without losing anything, because
/// [`crate::auth::csrf`] already refuses a mutating request whose body is not
/// `application/json`, so they are unreachable behind the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    axum::Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        // `request_path`, not `req.uri()`: every mutating route is nested under
        // `/api/v1`, where the prefix has been stripped by the time this runs.
        let path = request_path(req.extensions(), req.uri());
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(rejected_body(&rejection).with_instance(path)),
        }
    }
}

/// Which problem one [`JsonRejection`] becomes.
///
/// `JsonRejection` is `#[non_exhaustive]`, so this cannot be an exhaustive match
/// — the `_` arm is the compiler's requirement, not a shortcut. Everything the
/// wildcard catches is a body kopiur-ui could not read, which is what
/// `invalid-body` says.
fn rejected_body(rejection: &JsonRejection) -> ApiError {
    match rejection {
        // axum's own body-length limit tripped. The body was never parsed, so
        // serde has nothing to say about it and `body_text()` is about the
        // length rather than the content.
        JsonRejection::BytesRejection(_) => body_too_large(rejection.body_text()),
        _ => invalid_body(rejection.body_text()),
    }
}

/// The 400 every rejected body — malformed JSON, an unknown field, a wrong type,
/// or a field combination the CRD cannot express — is answered with.
fn invalid_body(detail: impl Into<String>) -> ApiError {
    problem(
        400,
        "invalid-body",
        format!("kopiur-ui could not read the request: {}", detail.into()),
        "The request body did not match what this endpoint accepts, so nothing was created \
         or changed.",
        "correct the request and try again; if you did not hand-write it, reload the page so \
         the SPA bundle matches this backend",
    )
}

/// The 413 for a body that exceeded the server's length limit.
///
/// Distinct from [`invalid_body`] because the remediation is the opposite one:
/// nothing about the request's *shape* is wrong, so telling the caller to fix
/// their JSON would send them hunting for a defect that is not there.
fn body_too_large(detail: impl Into<String>) -> ApiError {
    problem(
        413,
        "body-too-large",
        format!(
            "kopiur-ui refused the request because its body is too large: {}",
            detail.into()
        ),
        "The body exceeded the server's request-size limit and was never read, so nothing \
         was created or changed. Every action body here is a handful of fields; a large one \
         usually means a field was filled with a file or a whole manifest.",
        "send only the fields the action needs; if a list in the request is genuinely that \
         long, split it across several requests",
    )
}

// --- shared plumbing --------------------------------------------------------

/// The per-request `kopiur_ops` context: the caller's impersonating client, the
/// namespace the action names, and the UI's field manager.
///
/// Built fresh per request rather than held in state — the client is the
/// caller's, so it can never be reused for a different identity, and the scope is
/// whatever this one body asked for.
fn ops_ctx(app: &AppState, id: &Identity, namespace: &str) -> Result<OpsCtx, ApiError> {
    let client = app.auth.clients.client_for(id).map_err(|e| {
        problem(
            500,
            "impersonation-failed",
            "kopiur-ui could not build a Kubernetes client for your identity.",
            format!("Constructing the impersonating client failed: {e}"),
            "retry; if it persists, check the kopiur-ui pod's logs and that its ServiceAccount \
             token is mounted",
        )
    })?;
    Ok(OpsCtx {
        client,
        namespace: namespace.to_string(),
        scope: Scope::Namespace(namespace.to_string()),
        field_manager: FIELD_MANAGER.to_string(),
    })
}

/// The `OpsError` a missing object produces, so a 404 raised here renders exactly
/// like a 404 the apiserver produced.
fn not_found(kind: &'static str, plural: &'static str, namespace: &str, name: &str) -> OpsError {
    OpsError::NotFound {
        kind,
        plural,
        name: name.to_string(),
        scope: scope_suffix(Some(namespace)),
        scope_flag: format!(" -n {namespace}"),
    }
}

/// A just-created object as the wire reference the SPA links to.
///
/// Generic over the kind because `Snapshot` and `Restore` need the identical
/// treatment and a second hand-written copy is how the two would drift.
///
/// The two absent-metadata cases are deliberately not symmetric. A missing
/// namespace is *recoverable*: the action ran in exactly one namespace, so
/// `fallback_namespace` is not a guess, it is the answer. A missing name is not
/// — there is nothing to substitute, and `{"name": ""}` would hand the SPA a
/// link that resolves to nothing. The apiserver always echoes a name on a
/// created object, so this is unreachable in practice; making it an `internal`
/// problem keeps the one impossible state from degrading into a wrong answer.
fn created_ref<K: kube::Resource<DynamicType = ()>>(
    object: &K,
    fallback_namespace: &str,
) -> Result<SnapshotRefView, ApiError> {
    let meta = object.meta();
    let name = meta.name.clone().ok_or_else(|| {
        problem(
            500,
            "internal",
            format!(
                "kopiur-ui created a {} but the API server returned it without a name.",
                K::kind(&())
            ),
            "A created object always echoes metadata.name, so this should be impossible; \
             kopiur-ui has nothing to link the new object by.",
            "the object was most likely created — check the namespace for it before retrying, \
             and report this at https://github.com/home-operations/kopiur/issues",
        )
    })?;

    Ok(SnapshotRefView {
        namespace: meta
            .namespace
            .clone()
            .unwrap_or_else(|| fallback_namespace.to_string()),
        name,
    })
}

/// A receipt for an action that stamped a request rather than creating anything.
fn requested(kind: impl Into<String>, requested_at: String, note: Option<String>) -> ActionReceipt {
    ActionReceipt {
        kind: kind.into(),
        created: Vec::new(),
        requested_at: Some(requested_at),
        note,
    }
}

// --- string -> closed enum --------------------------------------------------

/// Accepted spellings for one suspendable kind: the kebab-case token the SPA
/// sends, and the lowercased CRD kind a hand-written request is likely to use.
const SUSPENDABLE_KINDS: &[(&str, &str, SuspendableKind)] = &[
    ("policy", "snapshotpolicy", SuspendableKind::Policy),
    ("schedule", "snapshotschedule", SuspendableKind::Schedule),
    ("repository", "repository", SuspendableKind::Repository),
    (
        "cluster-repository",
        "clusterrepository",
        SuspendableKind::ClusterRepository,
    ),
    (
        "replication",
        "repositoryreplication",
        SuspendableKind::Replication,
    ),
    (
        "snapshot-replication",
        "snapshotreplication",
        SuspendableKind::SnapshotReplication,
    ),
];

/// Parse `SuspendBody::kind`.
///
/// Case-insensitive over both spellings in [`SUSPENDABLE_KINDS`], because the
/// field's own contract calls it "the CRD kind" while the SPA sends the
/// kebab-case token; accepting one and rejecting the other would fail a correct
/// request over a cosmetic difference. Anything else is a 400 listing every
/// accepted token — never a default, because suspending the wrong kind of object
/// stops backups the caller did not ask to stop.
fn suspendable_kind(raw: &str) -> Result<SuspendableKind, ApiError> {
    let want = raw.trim().to_ascii_lowercase();
    SUSPENDABLE_KINDS
        .iter()
        .find(|(token, crd, _)| *token == want || *crd == want)
        .map(|(_, _, kind)| *kind)
        .ok_or_else(|| {
            invalid_body(format!(
                "{raw:?} is not a suspendable kind; expected one of {}",
                SUSPENDABLE_KINDS
                    .iter()
                    .map(|(token, _, _)| *token)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// Parse a repository kind — `ScanCatalogBody::kind` and
/// `RepositoryRefBody::kind`.
///
/// Same two spellings as [`suspendable_kind`], for the same reason.
fn repository_kind(raw: &str) -> Result<RepositoryKind, ApiError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "repository" => Ok(RepositoryKind::Repository),
        "cluster-repository" | "clusterrepository" => Ok(RepositoryKind::ClusterRepository),
        _ => Err(invalid_body(format!(
            "{raw:?} is not a repository kind; expected repository or cluster-repository"
        ))),
    }
}

/// Parse `MaintenanceRunBody::mode` with the operator's own annotation parser, so
/// the UI cannot accept a mode the reconciler would not honor.
fn manual_run_mode(raw: &str) -> Result<ManualRunMode, ApiError> {
    ManualRunMode::parse(raw.trim()).ok_or_else(|| {
        invalid_body(format!(
            "{raw:?} is not a maintenance run mode; expected quick or full"
        ))
    })
}

/// The namespace a namespaced action runs in, refusing an empty one.
///
/// Every namespace input in this module goes through here, including the four
/// that are a required `String` on the wire. Required is not the same as
/// non-empty: a Helm chart renders a nulled value as `""`, and a hand-written
/// request can send it outright. `Api::namespaced(client, "")` does not fail —
/// it builds `.../namespaces//<plural>`, a URL naming a *different* collection —
/// so an empty namespace has to be a 400 rather than a request against whatever
/// that resolves to.
fn require_namespace<'a>(kind_label: &str, namespace: &'a str) -> Result<&'a str, ApiError> {
    if namespace.is_empty() {
        return Err(invalid_body(format!(
            "namespace is required for {kind_label}, which is a namespaced kind"
        )));
    }
    Ok(namespace)
}

/// [`require_namespace`] for the two bodies whose namespace is optional because
/// the kind may be cluster-scoped.
///
/// A cluster-scoped kind needs none, and one sent anyway is meaningless rather
/// than malformed — the CRD has no namespace for it to disagree with — so it is
/// dropped instead of refused.
fn action_namespace(
    namespaced: bool,
    kind_label: &str,
    namespace: Option<&str>,
) -> Result<Option<String>, ApiError> {
    if !namespaced {
        return Ok(None);
    }
    require_namespace(kind_label, namespace.unwrap_or_default()).map(|ns| Some(ns.to_string()))
}

/// Which [`MaintenanceTarget`] a run request names.
///
/// Exactly one of `name` and `repository`: naming both is ambiguous (they can
/// disagree, and firing at a guess would run maintenance on the wrong
/// repository), naming neither says nothing at all. Both are 400s rather than a
/// precedence rule, so the caller learns which field to drop.
fn maintenance_target(body: &MaintenanceRunBody) -> Result<MaintenanceTarget, ApiError> {
    match (body.name.as_deref(), body.repository.as_ref()) {
        (Some(name), None) => Ok(MaintenanceTarget::Named(name.to_string())),
        (None, Some(repo)) => Ok(MaintenanceTarget::ByRepository {
            kind: repository_kind(&repo.kind)?,
            name: repo.name.clone(),
        }),
        (Some(_), Some(_)) => Err(invalid_body(
            "name and repository both name a maintenance target; send exactly one",
        )),
        (None, None) => Err(invalid_body(
            "neither name nor repository was given; send exactly one to say which Maintenance \
             to run",
        )),
    }
}

/// Map a wire repository reference onto the CRD's own.
fn repository_ref(body: &RepositoryRefBody) -> Result<RepositoryRef, ApiError> {
    let kind = repository_kind(&body.kind)?;
    Ok(RepositoryRef {
        kind,
        name: body.name.clone(),
        // Meaningless for a ClusterRepository (and forbidden on the CRD), so it
        // is dropped here rather than sent for the apiserver to reject.
        namespace: match kind {
            RepositoryKind::Repository => body.namespace.clone(),
            RepositoryKind::ClusterRepository => None,
        },
    })
}

// --- restore mapping --------------------------------------------------------

/// Map [`RestoreSourceBody`] onto the CRD's `RestoreSource`.
///
/// `source_path` is `RestoreBody::source_path`: a **source selector**, not a
/// subtree filter. It says which of a repository's kopia source paths to READ
/// from — the case that needs it is a multi-PVC `pvcSelector` policy, where each
/// member wrote its own `/pvc/<name>` source and the restore has to name the one
/// it wants. It does not change what gets written, and it cannot restore part of
/// a snapshot; `FromPolicy::source_path` says so in the CRD itself.
///
/// It is a property of the *source*, so where it lands depends on which source
/// this is — and one of the three has no selector at all:
///
/// * `fromPolicy` — fills [`FromPolicy::source_path`], overriding the path
///   kopiur would otherwise derive from the policy's `sourcePathStrategy`.
/// * `identity` — the variant carries its own `sourcePath` (it is a component of
///   the kopia identity being addressed); the body-level one fills it in only
///   when the variant left it unset, so the more specific value always wins.
/// * `snapshotRef` — refused. A `Snapshot` *is* one already-selected source, so
///   there is no selector to set; the CRD gives `RestoreSource::SnapshotRef` only
///   a name and a namespace. Accepting the field and dropping it would let a
///   caller believe they had picked a source when the snapshot they named had
///   already picked it for them — the CLI refuses the same combination
///   (`--source-path` is `fromPolicy`-only).
fn restore_source(
    body: &RestoreSourceBody,
    source_path: Option<&str>,
) -> Result<RestoreSource, ApiError> {
    match body {
        RestoreSourceBody::SnapshotRef { name, namespace } => {
            if source_path.is_some() {
                return Err(invalid_body(
                    "sourcePath selects which kopia source to read from, and a snapshotRef \
                     source has no such selector — the Snapshot you named is already one \
                     source. Drop sourcePath to restore that snapshot, or use a fromPolicy \
                     or identity source to choose a different source path",
                ));
            }
            Ok(RestoreSource::SnapshotRef(ObjectRef {
                name: name.clone(),
                namespace: namespace.clone(),
            }))
        }
        RestoreSourceBody::FromPolicy {
            name,
            namespace,
            as_of,
            offset,
        } => Ok(RestoreSource::FromPolicy(FromPolicy {
            name: name.clone(),
            namespace: namespace.clone(),
            as_of: as_of.clone(),
            // The CRD's own default: 0 is the latest snapshot.
            offset: i64::from(offset.unwrap_or(0)),
            source_path: source_path.map(str::to_string),
        })),
        RestoreSourceBody::Identity {
            username,
            hostname,
            source_path: identity_path,
            snapshot_id,
        } => Ok(RestoreSource::Identity(IdentitySource {
            username: username.clone(),
            hostname: hostname.clone(),
            source_path: identity_path
                .clone()
                .or_else(|| source_path.map(str::to_string)),
            snapshot_id: snapshot_id.clone(),
            // Point-in-time selection over a raw identity is not on the UI's form
            // yet; `None` leaves the operator picking the newest match, which is
            // what an omitted field has always meant.
            as_of: None,
            offset: None,
        })),
    }
}

/// Map [`RestoreTargetBody`] onto the CRD's `RestoreTarget`.
///
/// The wire type has no populator variant on purpose: a populator restore is
/// claimed by a PVC's `dataSourceRef`, so it is authored alongside that PVC in
/// Git rather than fired from a button.
fn restore_target(body: &RestoreTargetBody) -> RestoreTarget {
    match body {
        RestoreTargetBody::PvcRef { name } => RestoreTarget::PvcRef(ObjectRef {
            name: name.clone(),
            // Always the Restore's own namespace: a restore writes into a claim
            // its mover Job can mount, and that Job runs where the Restore is.
            namespace: None,
        }),
        RestoreTargetBody::Pvc {
            name,
            storage_class_name,
            size,
        } => RestoreTarget::Pvc(PvcTemplate {
            name: name.clone(),
            storage_class_name: storage_class_name.clone(),
            capacity: Some(size.clone()),
            // Empty means "the CRD's default", which is `[ReadWriteOnce]`.
            access_modes: Vec::new(),
        }),
    }
}

/// `spec.options` for the one knob the UI exposes, or `None` when it was not set.
///
/// `RestoreBody::overwrite` is documented as "overwrite files that already exist
/// on the target", so it maps to `overwriteFiles` and nothing else — the
/// directory and symlink knobs keep kopia's defaults rather than being swept
/// along by a checkbox that never mentioned them.
fn restore_options(overwrite: Option<bool>) -> Option<RestoreOptions> {
    overwrite.map(|overwrite_files| RestoreOptions {
        overwrite_files: Some(overwrite_files),
        ..RestoreOptions::default()
    })
}

/// The full `kopiur_ops` request for one [`RestoreBody`].
fn restore_request(body: &RestoreBody) -> Result<RestoreRequest, ApiError> {
    Ok(RestoreRequest {
        source: restore_source(&body.source, body.source_path.as_deref())?,
        target: restore_target(&body.target),
        repository: body.repository.as_ref().map(repository_ref).transpose()?,
        options: restore_options(body.overwrite),
        // Missing-snapshot behavior, credential projection and the mover's
        // failure controls are deployment policy, not per-click choices: `None`
        // leaves the operator's defaults, exactly as a bare `kubectl kopiur
        // restore` does.
        policy: None,
        credential_projection: false,
        failure_policy: None,
        name: body.name.clone(),
    })
}

// --- handlers ---------------------------------------------------------------

/// `POST /actions/snapshot-now` — run a `SnapshotPolicy` right now.
///
/// The policy is read LIVE through the caller's impersonated client — never the
/// reflector cache, see the module header — and handed to the shared planner,
/// which mints the same fan-out cells a `SnapshotSchedule` slot would. `201` with
/// every `Snapshot` created; plural, because a `pvcSelector` or multi-repository
/// policy fans out.
async fn snapshot_now(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    ApiJson(body): ApiJson<SnapshotNowBody>,
) -> Result<(StatusCode, axum::Json<ActionReceipt>), ApiError> {
    let namespace = require_namespace("SnapshotPolicy", &body.namespace)?;
    let ctx = ops_ctx(&app, &id, namespace)?;

    // Live, through the caller's own client — never `app.source`. See the
    // module header: this object decides how many Snapshots get created and
    // against which repositories, so a reflector's watch lag here would create
    // the wrong backups rather than misrender a page.
    let policy: SnapshotPolicy = Api::<SnapshotPolicy>::namespaced(ctx.client.clone(), namespace)
        .get_opt(&body.policy)
        .await
        .map_err(|e| {
            classify_kube(
                "get",
                "SnapshotPolicy",
                "snapshotpolicies",
                Some(namespace),
                Some(&body.policy),
                e,
            )
        })?
        .ok_or_else(|| {
            not_found(
                "SnapshotPolicy",
                "snapshotpolicies",
                namespace,
                &body.policy,
            )
        })?;

    let request = SnapshotNowRequest {
        policy: body.policy.clone(),
        name: body.name.clone(),
        tags: body.tags.clone(),
        // The operator's origin-aware default decides the kopia snapshot's fate
        // when the CR is deleted; the UI does not override it from a form.
        deletion_policy: None,
        pin: body.pin,
        description: body.description.clone(),
        failure_policy: None,
        repository: body.repository.clone(),
    };

    let now = Utc::now();
    let planned = plan_snapshots(&ctx, &request, &policy, namespace, now).await?;
    let created = create_snapshots(&ctx, namespace, &planned).await?;

    Ok((
        StatusCode::CREATED,
        axum::Json(ActionReceipt {
            kind: "Snapshot".to_string(),
            created: created
                .iter()
                .map(|s| created_ref(s, namespace))
                .collect::<Result<Vec<_>, _>>()?,
            requested_at: None,
            note: None,
        }),
    ))
}

/// `POST /actions/restore` — create a `Restore`.
///
/// `201` with the created object's reference. Nothing is restored yet: the
/// operator resolves the source and runs the mover Job, and the SPA follows
/// `status.phase` from there.
async fn restore(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    ApiJson(body): ApiJson<RestoreBody>,
) -> Result<(StatusCode, axum::Json<ActionReceipt>), ApiError> {
    let namespace = require_namespace("Restore", &body.namespace)?;
    let ctx = ops_ctx(&app, &id, namespace)?;
    let request = restore_request(&body)?;
    let built = build_restore(&request, namespace, Utc::now());
    let created = create_restore(&ctx, namespace, built).await?;

    Ok((
        StatusCode::CREATED,
        axum::Json(ActionReceipt {
            kind: "Restore".to_string(),
            created: vec![created_ref(&created, namespace)?],
            requested_at: None,
            note: None,
        }),
    ))
}

/// `DELETE /snapshots/{namespace}/{name}` — delete a `Snapshot` CR.
///
/// Background propagation, because the kopia manifest's fate is decided by the
/// CR's finalizer and `deletionPolicy`, not by the apiserver's cascade — waiting
/// on foreground deletion would hold the request open for a mover Job.
///
/// `202`, never `204`: the object usually still exists when this returns. It is
/// re-read once for exactly that reason — a `Snapshot` held by the repository's
/// mass-deletion breaker keeps its [`DELETION_HELD_CONDITION`], and a UI that
/// reported a bare success there would tell the caller a backup was deleted while
/// the breaker was still holding it.
///
/// # `note` is a courtesy, not an answer — the SPA must watch the object
///
/// The re-read races the controller and usually loses. `DeletionHeld` is stamped
/// by a *later* reconcile, once the batched deleter has seen the
/// `deletionTimestamp`, so on a first DELETE the condition cannot be there yet
/// and this note will be absent. It fires mainly on a repeat DELETE of an
/// already-deleting object.
///
/// So: an ABSENT note means nothing at all. It does not mean the snapshot is
/// unheld, and a `202` here is not a statement that the kopia snapshot will be
/// deleted. Whoever renders this must follow the `Snapshot`'s own conditions for
/// the real answer and must not treat the receipt as authoritative for
/// `DeletionHeld`. A present note is trustworthy; its absence is not evidence.
async fn delete_snapshot(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<(StatusCode, axum::Json<ActionReceipt>), ApiError> {
    // The path parameter gets the same guard as every body namespace: a route
    // match is not a promise that the segment is non-empty.
    let namespace = require_namespace("Snapshot", &namespace)?;
    let ctx = ops_ctx(&app, &id, namespace)?;
    let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), namespace);

    api.delete(&name, &DeleteParams::background())
        .await
        .map_err(|e| {
            classify_kube(
                "delete",
                "Snapshot",
                "snapshots",
                Some(namespace),
                Some(&name),
                e,
            )
        })?;

    Ok((
        StatusCode::ACCEPTED,
        axum::Json(ActionReceipt {
            kind: "Snapshot".to_string(),
            created: Vec::new(),
            requested_at: None,
            note: deletion_hold_note(&api, &name).await,
        }),
    ))
}

/// The note explaining that a just-deleted `Snapshot` is being held back.
///
/// One re-read, best effort. The delete already succeeded, so a failure here
/// costs the caller an explanation, not the action — reporting a 500 for it would
/// claim the deletion failed when it did not. It is logged rather than swallowed.
async fn deletion_hold_note(api: &Api<Snapshot>, name: &str) -> Option<String> {
    let snapshot = match api.get_opt(name).await {
        Ok(found) => found?,
        Err(e) => {
            tracing::warn!(
                error = %e,
                snapshot = name,
                "the deletion was accepted but re-reading the Snapshot failed; a \
                 mass-deletion hold cannot be reported for it"
            );
            return None;
        }
    };
    hold_note(&snapshot)
}

/// **Pure.** The note for a `Snapshot` the mass-deletion breaker is holding.
///
/// Split from the re-read so the *decision* is fixture-testable: this string is
/// what the SPA renders as the answer to "the snapshot is still there, why?",
/// and a receipt that lost it would leave a `202` looking like a completed
/// deletion. The condition is [`DELETION_HELD_CONDITION`] at `True` — the same
/// condition the controller writes and `kubectl kopiur` reads, not a second
/// opinion about what "held" means.
fn hold_note(snapshot: &Snapshot) -> Option<String> {
    let held = snapshot
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or_default()
        .iter()
        .any(|c| c.type_ == DELETION_HELD_CONDITION && c.status == "True");

    held.then(|| {
        "deletion is held by the repository's mass-deletion breaker; a cluster admin can \
         release it with the allow-mass-deletion annotation"
            .to_string()
    })
}

/// `POST /actions/suspend` — flip one object's `spec.suspend`.
///
/// `200`, not `202`: the flip is the whole action and it is complete when the
/// patch returns. The value is explicit rather than a toggle, so two people
/// clicking at once converge instead of undoing each other, and asking for the
/// value already in place writes nothing at all.
async fn set_suspended(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    ApiJson(body): ApiJson<SuspendBody>,
) -> Result<axum::Json<ActionReceipt>, ApiError> {
    let kind = suspendable_kind(&body.kind)?;
    let meta = suspend::kind_meta(kind);
    let namespace = action_namespace(
        kind != SuspendableKind::ClusterRepository,
        meta.kind,
        body.namespace.as_deref(),
    )?;

    // A cluster-scoped flip never reads the context namespace (`kopiur_ops` takes
    // the `Api::all` path for it), so the empty string here is unused rather than
    // a namespace that could be looked up.
    let ctx = ops_ctx(&app, &id, namespace.as_deref().unwrap_or_default())?;
    let report =
        suspend::set_suspended(&ctx, kind, namespace.as_deref(), &body.name, body.suspend).await?;

    Ok(axum::Json(ActionReceipt {
        kind: report.kind.to_string(),
        created: Vec::new(),
        requested_at: None,
        note: Some(suspend_note(&report)),
    }))
}

/// What the flip actually did, including "nothing" — an idempotent no-op is a
/// success the caller should be able to tell apart from a change.
fn suspend_note(report: &SuspendReport) -> String {
    let scope = report
        .namespace
        .as_deref()
        .map(|ns| format!(" in namespace {ns}"))
        .unwrap_or_default();
    let verb = if report.desired {
        "suspended"
    } else {
        "resumed"
    };
    if report.previous == report.desired {
        format!(
            "{}/{}{scope} was already {verb}; nothing was changed",
            report.kind, report.name
        )
    } else {
        format!("{}/{}{scope} is now {verb}", report.kind, report.name)
    }
}

/// `POST /actions/maintenance-run` — ask for a maintenance run now.
///
/// `202`: this stamps the run-request annotation the `Maintenance` reconciler
/// honors, and the returned `requestedAt` is the token `status.manualRun` echoes
/// back — the SPA matches on it so a previous run's outcome cannot be mistaken
/// for this one's.
async fn maintenance_run(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    ApiJson(body): ApiJson<MaintenanceRunBody>,
) -> Result<(StatusCode, axum::Json<ActionReceipt>), ApiError> {
    let target = maintenance_target(&body)?;
    let mode = manual_run_mode(&body.mode)?;
    let namespace = require_namespace("Maintenance", &body.namespace)?;
    let ctx = ops_ctx(&app, &id, namespace)?;

    let maint = maintenance::resolve(&ctx, &target).await?;
    let requested_at = maintenance::request_run(&ctx, &maint, mode, Utc::now()).await?;

    Ok((
        StatusCode::ACCEPTED,
        axum::Json(requested("Maintenance", requested_at, None)),
    ))
}

/// `POST /actions/replication-run` — ask for a replication run now.
///
/// `kind` is optional and detection is the default, because a caller who knows a
/// name usually does not care which of the two CRDs holds it. Detection reads
/// both kinds and refuses when one name exists in both rather than guessing which
/// replication to fire — and `kind` is how the caller answers that refusal, which
/// is why the remediation on `OpsError::AmbiguousTarget` names this field.
async fn replication_run(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    ApiJson(body): ApiJson<ReplicationRunBody>,
) -> Result<(StatusCode, axum::Json<ActionReceipt>), ApiError> {
    let stated = body.kind.as_deref().map(replication_kind).transpose()?;
    let namespace = require_namespace("replication resources", &body.namespace)?;
    let ctx = ops_ctx(&app, &id, namespace)?;

    let kind = match stated {
        Some(kind) => kind,
        None => replication::detect_kind(&ctx, &body.name).await?,
    };
    let requested_at = replication::request_run_by_kind(&ctx, kind, &body.name, Utc::now()).await?;

    Ok((
        StatusCode::ACCEPTED,
        axum::Json(requested(replication_kind_label(kind), requested_at, None)),
    ))
}

/// Accepted spellings for one replication kind: the kebab-case token and the
/// lowercased CRD kind, exactly as [`SUSPENDABLE_KINDS`] does it.
const REPLICATION_KINDS: &[(&str, &str, ReplicationKind)] = &[
    (
        "replication",
        "repositoryreplication",
        ReplicationKind::RepositoryReplication,
    ),
    (
        "snapshot-replication",
        "snapshotreplication",
        ReplicationKind::SnapshotReplication,
    ),
];

/// Parse `ReplicationRunBody::kind`.
///
/// Same two-spelling, case-insensitive rule as [`suspendable_kind`] — a caller
/// who writes `RepositoryReplication` for one endpoint and `replication` for the
/// other should not have to remember which endpoint wanted which. Unknown input
/// is a 400 listing the accepted tokens rather than a silent fall back to
/// detection, because detection can pick the kind the caller was trying to rule
/// out.
fn replication_kind(raw: &str) -> Result<ReplicationKind, ApiError> {
    let want = raw.trim().to_ascii_lowercase();
    REPLICATION_KINDS
        .iter()
        .find(|(token, crd, _)| *token == want || *crd == want)
        .map(|(_, _, kind)| *kind)
        .ok_or_else(|| {
            invalid_body(format!(
                "{raw:?} is not a replication kind; expected one of {}, or omit kind to \
                 detect it from the name",
                REPLICATION_KINDS
                    .iter()
                    .flat_map(|(token, _, kind)| [*token, replication_kind_label(*kind)])
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// The CRD kind name for a replication kind. Exhaustive over [`ReplicationKind`],
/// so a third replication kind cannot compile until the receipt names it.
fn replication_kind_label(kind: ReplicationKind) -> &'static str {
    match kind {
        ReplicationKind::RepositoryReplication => "RepositoryReplication",
        ReplicationKind::SnapshotReplication => "SnapshotReplication",
    }
}

/// `POST /actions/scan-catalog` — re-materialize a repository's discovered
/// snapshots now.
///
/// `202` with the scan token that was stamped. The token is second-precision, so
/// a burst of clicks collapses onto one honored scan rather than starting a
/// bootstrap Job per click.
async fn scan_catalog(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    ApiJson(body): ApiJson<ScanCatalogBody>,
) -> Result<(StatusCode, axum::Json<ActionReceipt>), ApiError> {
    let kind = repository_kind(&body.kind)?;
    let namespace = action_namespace(
        kind == RepositoryKind::Repository,
        kind.kind_str(),
        body.namespace.as_deref(),
    )?;

    // As in `set_suspended`: the ClusterRepository path is `Api::all`, so the
    // context namespace is never consulted for it.
    let ctx = ops_ctx(&app, &id, namespace.as_deref().unwrap_or_default())?;
    let token =
        catalog::request_scan(&ctx, kind, namespace.as_deref(), &body.name, Utc::now()).await?;

    Ok((
        StatusCode::ACCEPTED,
        axum::Json(requested(kind.kind_str(), token, None)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode as Status};
    use axum::routing::get;
    use http_body_util::BodyExt as _;
    use kopiur_ui_model::problem::Problem;
    use tower::ServiceExt as _;

    use crate::auth::csrf;

    // --- helpers ------------------------------------------------------------

    /// Round-trip a JSON value through the wire type, the way the apiserver's
    /// clients do — never `serde_yaml` straight into a typed value (see the
    /// api-conventions rule on externally-tagged enums).
    fn body_from(value: serde_json::Value) -> RestoreBody {
        serde_json::from_value(value).expect("a RestoreBody fixture must deserialize")
    }

    /// The `Restore` one body would create, as JSON — the shape that actually
    /// reaches the apiserver.
    fn built_spec(value: serde_json::Value) -> serde_json::Value {
        let body = body_from(value);
        let request = restore_request(&body).expect("the fixture must map cleanly");
        let now = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 9, 8, 12, 0, 0).unwrap();
        let restore = build_restore(&request, &body.namespace, now);
        serde_json::to_value(&restore).expect("a Restore serializes")
    }

    /// The minimal `pvcRef` target, so a source-focused fixture stays readable.
    fn pvc_ref_target() -> serde_json::Value {
        serde_json::json!({ "pvcRef": { "name": "data" } })
    }

    /// The minimal `snapshotRef` source, for the target-focused fixtures.
    fn snapshot_ref_source() -> serde_json::Value {
        serde_json::json!({ "snapshotRef": { "name": "nightly-1" } })
    }

    async fn problem_of(response: axum::response::Response) -> Problem {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("problem+json")
    }

    /// An app carrying only this module's router, mounted the way [`crate::app`]
    /// mounts it: the CSRF guard inside, the identity middleware outside.
    ///
    /// The middleware matters even though no test here reaches a cluster —
    /// `CurrentIdentity` is a `FromRequestParts` extractor, so it runs *before*
    /// the body is read. Without it every request would stop at a 500 and the
    /// body-rejection path below would never be exercised. The default
    /// [`crate::auth::AuthState`] identifies every caller as one inert anonymous
    /// user whose client can reach nothing, which is exactly what these tests
    /// want: identity resolves, and any handler that gets as far as the
    /// apiserver fails to connect instead of touching a real cluster.
    fn app() -> Router {
        let state = test_state();
        Router::new()
            .merge(router())
            // Present only so a 404 in these tests means "no such route" rather
            // than "the router has no fallback".
            .route("/unclaimed", get(|| async { "unclaimed" }))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::auth::identity_middleware,
            ))
            .with_state(state)
    }

    fn test_state() -> AppState {
        use crate::config::*;
        let provider = Arc::new(kopiur_telemetry::MetricsProvider::new("kopiur-ui-test"));
        AppState {
            cfg: Arc::new(UiConfig {
                addr: DEFAULT_ADDR.parse().expect("default addr"),
                ops_addr: DEFAULT_OPS_ADDR.parse().expect("default ops addr"),
                auth: AuthConfig {
                    mode: AuthMode::AnonymousOnly(AnonymousIdentity {
                        user: "viewer".to_string(),
                        groups: Vec::new(),
                    }),
                    groups_separator: DEFAULT_GROUPS_SEPARATOR.to_string(),
                    email_header: None,
                    extra_keys: Vec::new(),
                    allowed_groups: None,
                    proxy_secret: None,
                },
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
            }),
            metrics: Arc::new(crate::metrics::UiMetrics::new(provider)),
            readiness: Arc::new(crate::ops_listener::Readiness::new(
                crate::static_files::is_placeholder(),
            )),
            // `AuthState::new`, not `unconfigured()`: the latter refuses every
            // request with `NotWired` (a deliberate fail-closed), which would
            // stop each of these tests at a 500 before the body is ever read.
            // The base config still points at 127.0.0.1:1, so a handler that
            // reaches the apiserver fails to connect rather than touching a
            // cluster.
            auth: Arc::new(crate::auth::AuthState::new(
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
                },
                kube::Config::new("http://127.0.0.1:1/".parse().expect("test url")),
                CacheLimits {
                    size: 8,
                    ttl: std::time::Duration::from_secs(600),
                },
            )),
            source: Arc::new(crate::cache::Source::Impersonated),
            sessions: Arc::new(crate::browse::session_pool::SessionPool::default()),
        }
    }

    /// A request the CSRF guard lets through.
    fn spa_request(method: &str, uri: &str, body: &str) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder()
            .method(method)
            .uri(uri)
            .header(csrf::REQUEST_HEADER, csrf::REQUEST_HEADER_VALUE)
            .header("sec-fetch-site", "same-origin");
        if !body.is_empty() {
            builder = builder.header("content-type", "application/json");
        }
        builder
            .body(Body::from(body.to_string()))
            .expect("test request")
    }

    // --- routing + the guard ------------------------------------------------

    #[tokio::test]
    async fn every_mutating_route_exists_and_sits_behind_the_csrf_guard() {
        // Without the SPA's marker header a mutation is refused before any
        // handler runs; with it, the route is matched (and then stops at the
        // identity extractor, which this bare router has no middleware for).
        let routes = [
            ("POST", "/actions/snapshot-now", "{}"),
            ("POST", "/actions/restore", "{}"),
            ("POST", "/actions/suspend", "{}"),
            ("POST", "/actions/maintenance-run", "{}"),
            ("POST", "/actions/replication-run", "{}"),
            ("POST", "/actions/scan-catalog", "{}"),
            ("DELETE", "/snapshots/prod/nightly-1", ""),
        ];

        for (method, uri, body) in routes {
            let mut unguarded = HttpRequest::builder().method(method).uri(uri);
            if !body.is_empty() {
                unguarded = unguarded.header("content-type", "application/json");
            }
            let response = app()
                .oneshot(unguarded.body(Body::from(body)).expect("request"))
                .await
                .expect("response");
            assert_eq!(
                response.status(),
                Status::FORBIDDEN,
                "{method} {uri} must be gated by the CSRF guard"
            );
            assert_eq!(problem_of(response).await.r#type, "urn:kopiur:problem:csrf");

            let response = app()
                .oneshot(spa_request(method, uri, body))
                .await
                .expect("response");
            assert_ne!(
                response.status(),
                Status::NOT_FOUND,
                "{method} {uri} must be a real route"
            );
            assert_ne!(
                response.status(),
                Status::METHOD_NOT_ALLOWED,
                "{method} {uri} must accept that method"
            );
        }
    }

    #[tokio::test]
    async fn a_safe_method_on_a_mutating_path_is_still_not_a_route() {
        // The guard lets safe methods through untouched, so this proves the 405
        // comes from the router rather than from the guard swallowing it.
        let response = app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/actions/snapshot-now")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), Status::METHOD_NOT_ALLOWED);
    }

    // --- ApiJson ------------------------------------------------------------

    /// Drive `ApiJson` through a real route so the rejection path is the one
    /// axum actually takes.
    async fn reject(body: &str) -> Problem {
        let response = app()
            .oneshot(spa_request("POST", "/actions/suspend", body))
            .await
            .expect("response");
        assert_eq!(response.status(), Status::BAD_REQUEST, "body: {body}");
        problem_of(response).await
    }

    #[tokio::test]
    async fn an_unknown_field_is_a_400_naming_the_field() {
        let problem = reject(
            r#"{"kind":"policy","name":"nightly","namespace":"prod","suspend":true,"typo":1}"#,
        )
        .await;

        assert_eq!(problem.r#type, "urn:kopiur:problem:invalid-body");
        assert_eq!(problem.status, 400);
        assert!(
            problem.what.contains("typo"),
            "serde names the offending field and the message must carry it: {}",
            problem.what
        );
        assert_eq!(problem.instance.as_deref(), Some("/actions/suspend"));
        assert!(!problem.fix.is_empty(), "every problem carries a fix");
    }

    #[tokio::test]
    async fn a_wrong_type_is_a_400_naming_the_field() {
        let problem =
            reject(r#"{"kind":"policy","name":"nightly","namespace":"prod","suspend":"yes"}"#)
                .await;

        assert_eq!(problem.r#type, "urn:kopiur:problem:invalid-body");
        assert!(
            problem.what.contains("suspend"),
            "the message must say which field had the wrong type: {}",
            problem.what
        );
    }

    #[tokio::test]
    async fn a_missing_required_field_and_malformed_json_are_both_400s() {
        for body in [r#"{"kind":"policy"}"#, "{not json"] {
            let problem = reject(body).await;
            assert_eq!(problem.r#type, "urn:kopiur:problem:invalid-body");
            assert_eq!(problem.status, 400);
        }
    }

    // --- string -> enum tables ---------------------------------------------

    #[test]
    fn every_suspendable_kind_parses_from_both_spellings() {
        let cases = [
            ("policy", SuspendableKind::Policy),
            ("SnapshotPolicy", SuspendableKind::Policy),
            ("schedule", SuspendableKind::Schedule),
            ("SnapshotSchedule", SuspendableKind::Schedule),
            ("repository", SuspendableKind::Repository),
            ("Repository", SuspendableKind::Repository),
            ("cluster-repository", SuspendableKind::ClusterRepository),
            ("ClusterRepository", SuspendableKind::ClusterRepository),
            ("replication", SuspendableKind::Replication),
            ("RepositoryReplication", SuspendableKind::Replication),
            ("snapshot-replication", SuspendableKind::SnapshotReplication),
            ("SnapshotReplication", SuspendableKind::SnapshotReplication),
        ];
        for (raw, want) in cases {
            assert_eq!(
                suspendable_kind(raw).expect("a known spelling parses"),
                want,
                "{raw}"
            );
        }
    }

    #[test]
    fn the_suspendable_table_covers_every_variant_of_the_closed_enum() {
        // The table is data, so nothing in the compiler forces it to stay
        // complete; this is that check. Exhaustive `match` on the way in, so a
        // new variant fails to compile here until it is listed.
        for kind in [
            SuspendableKind::Policy,
            SuspendableKind::Schedule,
            SuspendableKind::Repository,
            SuspendableKind::ClusterRepository,
            SuspendableKind::Replication,
            SuspendableKind::SnapshotReplication,
        ] {
            let listed = SUSPENDABLE_KINDS.iter().any(|(_, _, k)| *k == kind);
            assert!(listed, "{kind:?} has no accepted spelling");
        }
        assert_eq!(SUSPENDABLE_KINDS.len(), 6);
    }

    #[test]
    fn an_unknown_suspendable_kind_is_a_400_listing_the_accepted_tokens() {
        for raw in ["", "snapshot", "Pod", "cluster repository", "polic"] {
            let error = suspendable_kind(raw).expect_err("must be refused");
            assert_eq!(error.status(), Status::BAD_REQUEST, "{raw:?}");
            assert_eq!(error.0.r#type, "urn:kopiur:problem:invalid-body");
            assert!(
                error.0.what.contains("cluster-repository"),
                "the refusal must list what IS accepted: {}",
                error.0.what
            );
        }
    }

    #[test]
    fn repository_kinds_parse_from_both_spellings_and_nothing_else() {
        for raw in ["repository", "Repository", " REPOSITORY "] {
            assert_eq!(
                repository_kind(raw).expect("parses"),
                RepositoryKind::Repository,
                "{raw:?}"
            );
        }
        for raw in ["cluster-repository", "ClusterRepository"] {
            assert_eq!(
                repository_kind(raw).expect("parses"),
                RepositoryKind::ClusterRepository,
                "{raw:?}"
            );
        }
        for raw in ["", "clusterrepo", "Snapshot", "repositories"] {
            let error = repository_kind(raw).expect_err("must be refused");
            assert_eq!(error.status(), Status::BAD_REQUEST, "{raw:?}");
            assert_eq!(error.0.r#type, "urn:kopiur:problem:invalid-body");
        }
    }

    #[test]
    fn run_modes_parse_exactly_the_two_the_reconciler_honors() {
        assert_eq!(
            manual_run_mode("quick").expect("parses"),
            ManualRunMode::Quick
        );
        assert_eq!(
            manual_run_mode(" full ").expect("parses"),
            ManualRunMode::Full
        );
        for raw in ["", "Quick", "FULL", "fast", "quick full"] {
            let error = manual_run_mode(raw).expect_err("must be refused");
            assert_eq!(error.status(), Status::BAD_REQUEST, "{raw:?}");
            assert!(
                error.0.what.contains("quick or full"),
                "the refusal names the accepted modes: {}",
                error.0.what
            );
        }
    }

    // --- namespace requirement ---------------------------------------------

    #[test]
    fn a_namespaced_kind_without_a_namespace_is_a_400_not_an_empty_url() {
        // An empty string is "unset" here exactly as it is everywhere else in
        // this crate: a Helm chart routinely renders a nulled value as `""`.
        for missing in [None, Some("")] {
            let error =
                action_namespace(true, "SnapshotPolicy", missing).expect_err("must be refused");
            assert_eq!(error.status(), Status::BAD_REQUEST);
            assert!(
                error.0.what.contains("SnapshotPolicy"),
                "the refusal names the kind: {}",
                error.0.what
            );
        }
        assert_eq!(
            action_namespace(true, "SnapshotPolicy", Some("prod")).expect("ok"),
            Some("prod".to_string())
        );
    }

    #[test]
    fn a_cluster_scoped_kind_needs_no_namespace_and_ignores_one_sent_anyway() {
        assert_eq!(
            action_namespace(false, "ClusterRepository", None).expect("ok"),
            None
        );
        assert_eq!(
            action_namespace(false, "ClusterRepository", Some("prod")).expect("ok"),
            None,
            "a ClusterRepository has no namespace for one to mean anything in"
        );
    }

    #[tokio::test]
    async fn a_required_namespace_sent_empty_is_a_400_not_a_request_to_an_empty_collection() {
        // `SnapshotNowBody::namespace` is a required `String`, which stops the
        // key being absent but not its value being `""` — and `Api::namespaced(
        // client, "")` would happily build `.../namespaces//snapshotpolicies`.
        // Every namespace input goes through `require_namespace` for this.
        let response = app()
            .oneshot(spa_request(
                "POST",
                "/actions/snapshot-now",
                r#"{"namespace":"","policy":"nightly","tags":[],"pin":false}"#,
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), Status::BAD_REQUEST);
        let problem = problem_of(response).await;
        assert_eq!(problem.r#type, "urn:kopiur:problem:invalid-body");
        assert!(
            problem.what.contains("namespace is required"),
            "{}",
            problem.what
        );
    }

    #[test]
    fn every_namespace_input_shares_one_guard() {
        for kind in ["SnapshotPolicy", "Restore", "Maintenance", "Snapshot"] {
            let error = require_namespace(kind, "").expect_err("empty must be refused");
            assert_eq!(error.status(), Status::BAD_REQUEST, "{kind}");
            assert!(error.0.what.contains(kind), "{}", error.0.what);
        }
        assert_eq!(require_namespace("Restore", "prod").expect("ok"), "prod");
    }

    // --- mutations never read from the cache --------------------------------

    #[test]
    fn no_mutation_reads_through_the_reflector_cache() {
        // The rule from the module header, asserted against the source itself:
        // a write-consequential read must not be served from a watch-fed store
        // that is one lag behind and SAR-gated with a TTL. `snapshot_now`'s
        // policy read is the one that matters — it decides how many Snapshots
        // get created — but the rule is module-wide so it stays greppable.
        let source = include_str!("mod.rs");
        let body = source
            .split("#[cfg(test)]")
            .next()
            .expect("the non-test half of this file");
        // Comment lines are skipped, so the module header may state the rule in
        // the same words the check looks for.
        let offending: Vec<&str> = body
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| line.contains("app.source") || line.contains("cache::Source"))
            .collect();
        assert!(
            offending.is_empty(),
            "actions/ must never read through crate::cache::Source: {offending:?}"
        );
    }

    // --- maintenance target -------------------------------------------------

    fn maintenance_body(
        name: Option<&str>,
        repository: Option<RepositoryRefBody>,
    ) -> MaintenanceRunBody {
        MaintenanceRunBody {
            namespace: "prod".to_string(),
            name: name.map(str::to_string),
            repository,
            mode: "quick".to_string(),
        }
    }

    fn repo_body(kind: &str, name: &str) -> RepositoryRefBody {
        RepositoryRefBody {
            kind: kind.to_string(),
            name: name.to_string(),
            namespace: None,
        }
    }

    #[test]
    fn a_maintenance_run_names_its_target_by_name_or_by_repository() {
        assert_eq!(
            maintenance_target(&maintenance_body(Some("nas-maint"), None)).expect("ok"),
            MaintenanceTarget::Named("nas-maint".to_string())
        );
        assert_eq!(
            maintenance_target(&maintenance_body(
                None,
                Some(repo_body("cluster-repository", "nas"))
            ))
            .expect("ok"),
            MaintenanceTarget::ByRepository {
                kind: RepositoryKind::ClusterRepository,
                name: "nas".to_string(),
            }
        );
    }

    #[test]
    fn naming_both_or_neither_maintenance_target_is_a_400() {
        let both = maintenance_target(&maintenance_body(
            Some("nas-maint"),
            Some(repo_body("repository", "nas")),
        ))
        .expect_err("both is ambiguous");
        assert_eq!(both.status(), Status::BAD_REQUEST);
        assert!(both.0.what.contains("exactly one"), "{}", both.0.what);

        let neither = maintenance_target(&maintenance_body(None, None)).expect_err("neither");
        assert_eq!(neither.status(), Status::BAD_REQUEST);
        assert!(neither.0.what.contains("exactly one"), "{}", neither.0.what);
    }

    #[test]
    fn a_maintenance_target_with_an_unknown_repository_kind_is_a_400() {
        let error = maintenance_target(&maintenance_body(None, Some(repo_body("Pod", "nas"))))
            .expect_err("must be refused");
        assert_eq!(error.status(), Status::BAD_REQUEST);
    }

    // --- restore mapping: 3 sources x 2 targets -----------------------------

    #[test]
    fn a_snapshot_ref_source_maps_to_the_crds_object_ref() {
        let spec = built_spec(serde_json::json!({
            "namespace": "prod",
            "source": { "snapshotRef": { "name": "nightly-1", "namespace": "backups" } },
            "target": pvc_ref_target(),
        }));
        assert_eq!(
            spec["spec"]["source"],
            serde_json::json!({ "snapshotRef": { "name": "nightly-1", "namespace": "backups" } })
        );
        assert_eq!(
            spec["spec"]["target"],
            serde_json::json!({ "pvcRef": { "name": "data" } })
        );
        assert_eq!(spec["metadata"]["namespace"], "prod");
    }

    #[test]
    fn a_from_policy_source_carries_as_of_offset_and_the_body_source_path() {
        let spec = built_spec(serde_json::json!({
            "namespace": "prod",
            "source": {
                "fromPolicy": {
                    "name": "pg",
                    "namespace": "db",
                    "asOf": "2026-09-01T00:00:00Z",
                    "offset": 2,
                }
            },
            "target": pvc_ref_target(),
            "sourcePath": "/pvc/data/sub",
        }));
        assert_eq!(
            spec["spec"]["source"],
            serde_json::json!({
                "fromPolicy": {
                    "name": "pg",
                    "namespace": "db",
                    "asOf": "2026-09-01T00:00:00Z",
                    "offset": 2,
                    "sourcePath": "/pvc/data/sub",
                }
            })
        );
    }

    #[test]
    fn an_omitted_from_policy_offset_becomes_the_crds_default_of_latest() {
        let spec = built_spec(serde_json::json!({
            "namespace": "prod",
            "source": { "fromPolicy": { "name": "pg" } },
            "target": pvc_ref_target(),
        }));
        assert_eq!(
            spec["spec"]["source"]["fromPolicy"]["offset"], 0,
            "0 is the latest snapshot"
        );
        assert!(
            spec["spec"]["source"]["fromPolicy"]
                .get("sourcePath")
                .is_none(),
            "an unset path must not be serialized as null"
        );
    }

    #[test]
    fn an_identity_source_maps_every_component_and_leaves_time_travel_unset() {
        let spec = built_spec(serde_json::json!({
            "namespace": "prod",
            "source": {
                "identity": {
                    "username": "kopiur",
                    "hostname": "nas",
                    "sourcePath": "/pvc/data",
                    "snapshotId": "k1234",
                }
            },
            "target": pvc_ref_target(),
        }));
        assert_eq!(
            spec["spec"]["source"],
            serde_json::json!({
                "identity": {
                    "username": "kopiur",
                    "hostname": "nas",
                    "sourcePath": "/pvc/data",
                    // The CRD renames this to `snapshotID`; a drift here would
                    // silently stop pinning the manifest.
                    "snapshotID": "k1234",
                }
            })
        );
    }

    #[test]
    fn the_body_source_path_fills_an_identity_that_left_its_own_unset() {
        let spec = built_spec(serde_json::json!({
            "namespace": "prod",
            "source": { "identity": { "username": "kopiur", "hostname": "nas" } },
            "target": pvc_ref_target(),
            "sourcePath": "/pvc/data",
        }));
        assert_eq!(
            spec["spec"]["source"]["identity"]["sourcePath"],
            "/pvc/data"
        );
    }

    #[test]
    fn an_identitys_own_source_path_wins_over_the_body_level_one() {
        let spec = built_spec(serde_json::json!({
            "namespace": "prod",
            "source": {
                "identity": { "username": "kopiur", "hostname": "nas", "sourcePath": "/specific" }
            },
            "target": pvc_ref_target(),
            "sourcePath": "/general",
        }));
        assert_eq!(
            spec["spec"]["source"]["identity"]["sourcePath"],
            "/specific"
        );
    }

    #[test]
    fn a_source_path_with_a_snapshot_ref_is_refused_rather_than_dropped() {
        // A Snapshot already pins the path it was written from, and there is no
        // field on `snapshotRef` for a second one. Ignoring it would restore the
        // whole snapshot while the caller believed a subtree was restored.
        let body = body_from(serde_json::json!({
            "namespace": "prod",
            "source": snapshot_ref_source(),
            "target": pvc_ref_target(),
            "sourcePath": "/pvc/data/sub",
        }));
        let error = restore_request(&body).expect_err("must be refused");
        assert_eq!(error.status(), Status::BAD_REQUEST);
        assert_eq!(error.0.r#type, "urn:kopiur:problem:invalid-body");
        assert!(
            error.0.what.contains("sourcePath"),
            "the refusal names the field: {}",
            error.0.what
        );
    }

    #[test]
    fn a_created_pvc_target_maps_size_onto_the_crds_capacity() {
        let spec = built_spec(serde_json::json!({
            "namespace": "prod",
            "source": snapshot_ref_source(),
            "target": {
                "pvc": { "name": "restored", "storageClassName": "fast", "size": "10Gi" }
            },
        }));
        assert_eq!(
            spec["spec"]["target"],
            serde_json::json!({
                "pvc": {
                    "name": "restored",
                    "storageClassName": "fast",
                    // The wire calls it `size`; the CRD calls it `capacity`.
                    "capacity": "10Gi",
                }
            })
        );
    }

    #[test]
    fn a_created_pvc_without_a_storage_class_leaves_the_cluster_default() {
        let spec = built_spec(serde_json::json!({
            "namespace": "prod",
            "source": snapshot_ref_source(),
            "target": { "pvc": { "name": "restored", "size": "1Gi" } },
        }));
        assert_eq!(
            spec["spec"]["target"],
            serde_json::json!({ "pvc": { "name": "restored", "capacity": "1Gi" } })
        );
    }

    #[test]
    fn every_source_and_target_combination_builds_a_restore() {
        let sources = [
            ("snapshotRef", snapshot_ref_source()),
            (
                "fromPolicy",
                serde_json::json!({ "fromPolicy": { "name": "pg" } }),
            ),
            (
                "identity",
                serde_json::json!({ "identity": { "username": "u", "hostname": "h" } }),
            ),
        ];
        let targets = [
            ("pvcRef", pvc_ref_target()),
            (
                "pvc",
                serde_json::json!({ "pvc": { "name": "restored", "size": "1Gi" } }),
            ),
        ];

        for (source_key, source) in &sources {
            for (target_key, target) in &targets {
                let spec = built_spec(serde_json::json!({
                    "namespace": "prod",
                    "source": source,
                    "target": target,
                }));
                assert!(
                    spec["spec"]["source"].get(source_key).is_some(),
                    "{source_key} x {target_key}: source"
                );
                assert!(
                    spec["spec"]["target"].get(target_key).is_some(),
                    "{source_key} x {target_key}: target"
                );
                // Nothing the UI does not set may appear on the wire.
                for absent in ["policy", "credentialProjection", "mover", "failurePolicy"] {
                    assert!(
                        spec["spec"].get(absent).is_none(),
                        "{source_key} x {target_key}: {absent} must be left to the operator"
                    );
                }
            }
        }
    }

    #[test]
    fn overwrite_maps_to_overwrite_files_alone_and_is_absent_when_unset() {
        let base = serde_json::json!({
            "namespace": "prod",
            "source": snapshot_ref_source(),
            "target": pvc_ref_target(),
        });
        assert!(
            built_spec(base.clone())["spec"].get("options").is_none(),
            "an unset overwrite carries no options at all"
        );

        for wanted in [true, false] {
            let mut value = base.clone();
            value["overwrite"] = serde_json::Value::Bool(wanted);
            assert_eq!(
                built_spec(value)["spec"]["options"],
                serde_json::json!({ "overwriteFiles": wanted }),
                "the directory and symlink knobs keep kopia's defaults"
            );
        }
    }

    #[test]
    fn a_repository_ref_drops_the_namespace_for_a_cluster_repository() {
        let resolved = repository_ref(&RepositoryRefBody {
            kind: "cluster-repository".to_string(),
            name: "nas".to_string(),
            namespace: Some("prod".to_string()),
        })
        .expect("parses");
        assert_eq!(resolved.kind, RepositoryKind::ClusterRepository);
        assert_eq!(resolved.namespace, None);

        let resolved = repository_ref(&RepositoryRefBody {
            kind: "repository".to_string(),
            name: "nas".to_string(),
            namespace: Some("backups".to_string()),
        })
        .expect("parses");
        assert_eq!(resolved.namespace.as_deref(), Some("backups"));
    }

    #[test]
    fn a_named_restore_keeps_its_name_and_an_unnamed_one_is_derived() {
        let named = built_spec(serde_json::json!({
            "namespace": "prod",
            "source": snapshot_ref_source(),
            "target": pvc_ref_target(),
            "name": "my-restore",
        }));
        assert_eq!(named["metadata"]["name"], "my-restore");

        let derived = built_spec(serde_json::json!({
            "namespace": "prod",
            "source": snapshot_ref_source(),
            "target": pvc_ref_target(),
        }));
        assert_eq!(
            derived["metadata"]["name"],
            "restore-nightly-1-20260908120000"
        );
    }

    // --- receipts -----------------------------------------------------------

    #[test]
    fn a_receipt_serializes_camel_case_and_omits_what_was_not_set() {
        let receipt = ActionReceipt {
            kind: "Snapshot".to_string(),
            created: vec![SnapshotRefView {
                namespace: "prod".to_string(),
                name: "nightly-1".to_string(),
            }],
            requested_at: None,
            note: None,
        };
        assert_eq!(
            serde_json::to_value(&receipt).expect("serializes"),
            serde_json::json!({
                "kind": "Snapshot",
                "created": [{ "namespace": "prod", "name": "nightly-1" }],
            })
        );
    }

    #[test]
    fn a_requested_receipt_carries_requested_at_in_camel_case() {
        let receipt = requested(
            "Maintenance",
            "2026-09-08T12:00:00Z".to_string(),
            Some("held".to_string()),
        );
        assert_eq!(
            serde_json::to_value(&receipt).expect("serializes"),
            serde_json::json!({
                "kind": "Maintenance",
                "created": [],
                "requestedAt": "2026-09-08T12:00:00Z",
                "note": "held",
            })
        );
    }

    fn bare_snapshot() -> Snapshot {
        Snapshot::new(
            "nightly-1",
            kopiur_api::SnapshotSpec {
                policy_ref: None,
                repository: None,
                source: None,
                tags: None,
                failure_policy: None,
                deletion_policy: None,
                on_schedule_delete: None,
                pin: false,
                description: None,
            },
        )
    }

    #[test]
    fn a_created_reference_falls_back_to_the_namespace_the_action_ran_in() {
        let mut snapshot = bare_snapshot();
        assert_eq!(
            created_ref(&snapshot, "prod").expect("named").namespace,
            "prod"
        );
        snapshot.metadata.namespace = Some("backups".to_string());
        assert_eq!(
            created_ref(&snapshot, "prod").expect("named").namespace,
            "backups"
        );
    }

    #[test]
    fn the_same_helper_serves_every_created_kind() {
        // One generic, so `Snapshot` and `Restore` cannot drift apart.
        let restore = kopiur_api::Restore::new(
            "restore-1",
            kopiur_api::RestoreSpec {
                repository: None,
                source: RestoreSource::SnapshotRef(ObjectRef {
                    name: "nightly-1".to_string(),
                    namespace: None,
                }),
                target: RestoreTarget::PvcRef(ObjectRef {
                    name: "data".to_string(),
                    namespace: None,
                }),
                options: None,
                policy: None,
                credential_projection: None,
                mover: None,
                failure_policy: None,
            },
        );
        let view = created_ref(&restore, "prod").expect("named");
        assert_eq!(
            (view.namespace.as_str(), view.name.as_str()),
            ("prod", "restore-1")
        );
    }

    #[test]
    fn a_created_object_with_no_name_is_an_internal_problem_not_an_empty_link() {
        // Unreachable against a real apiserver, which always echoes a name. The
        // point is that the one impossible state fails loudly instead of handing
        // the SPA `{"name": ""}`, a link that resolves to nothing.
        let mut snapshot = bare_snapshot();
        snapshot.metadata.name = None;

        let error = created_ref(&snapshot, "prod").expect_err("must not degrade");
        assert_eq!(error.status(), Status::INTERNAL_SERVER_ERROR);
        assert_eq!(error.0.r#type, "urn:kopiur:problem:internal");
        assert!(error.0.what.contains("Snapshot"), "{}", error.0.what);
        assert!(!error.0.fix.is_empty());
    }

    #[test]
    fn the_suspend_note_distinguishes_a_change_from_an_idempotent_no_op() {
        let report = |previous: bool, desired: bool, namespace: Option<&str>| SuspendReport {
            meta: suspend::kind_meta(SuspendableKind::Policy),
            kind: "SnapshotPolicy",
            name: "nightly".to_string(),
            namespace: namespace.map(str::to_string),
            previous,
            desired,
            object: serde_json::Value::Null,
        };

        let changed = suspend_note(&report(false, true, Some("prod")));
        assert!(changed.contains("is now suspended"), "{changed}");
        assert!(changed.contains("in namespace prod"), "{changed}");

        let noop = suspend_note(&report(true, true, Some("prod")));
        assert!(noop.contains("was already suspended"), "{noop}");
        assert!(noop.contains("nothing was changed"), "{noop}");

        let resumed = suspend_note(&report(true, false, None));
        assert!(resumed.contains("is now resumed"), "{resumed}");
        assert!(
            !resumed.contains("namespace"),
            "a cluster-scoped object has no namespace to name: {resumed}"
        );
    }

    /// The delete receipt's `note` is the SPA's answer to "I deleted it, why is
    /// it still there?". A `DELETE` answers `202` whether or not the breaker is
    /// holding, so a receipt that lost this string would leave the UI saying
    /// "deleted" about a snapshot that is very much not gone.
    #[test]
    fn the_delete_receipt_carries_the_mass_deletion_breaker_text_when_it_holds() {
        let held: Snapshot = kopiur_api::testutil::from_yaml(&format!(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: {{ name: nightly-1, namespace: media }}
spec: {{}}
status:
  phase: Deleting
  conditions:
    - type: {DELETION_HELD_CONDITION}
      status: "True"
      reason: MassDeletionThresholdExceeded
      message: "37 snapshots would be deleted; the threshold is 10"
      lastTransitionTime: "2026-09-08T12:00:00Z"
"#
        ));
        let note = hold_note(&held).expect("a held deletion must explain itself");
        assert!(
            note.contains("mass-deletion breaker"),
            "the note must name the thing that is holding it: {note}"
        );
        assert!(
            note.contains("allow-mass-deletion"),
            "and the annotation that releases it: {note}"
        );

        // The receipt is what the SPA actually reads, so assert the shape it
        // lands in — a 202 whose `created` is empty and whose `note` explains.
        let receipt = ActionReceipt {
            kind: "Snapshot".to_string(),
            created: Vec::new(),
            requested_at: None,
            note: Some(note),
        };
        let body = serde_json::to_value(&receipt).expect("a receipt serializes");
        assert!(
            body["note"]
                .as_str()
                .is_some_and(|n| n.contains("allow-mass-deletion")),
            "{body}"
        );
    }

    /// An ABSENT note means nothing at all — see `delete_snapshot`'s own docs.
    /// It is not evidence the snapshot is unheld, so these two fixtures assert
    /// only that nothing is *invented*.
    #[test]
    fn a_snapshot_with_no_hold_condition_gets_no_note() {
        let unheld: Snapshot = kopiur_api::testutil::from_yaml(&format!(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: {{ name: nightly-1, namespace: media }}
spec: {{}}
status:
  phase: Deleting
  conditions:
    - type: {DELETION_HELD_CONDITION}
      status: "False"
      reason: BelowThreshold
      message: ""
      lastTransitionTime: "2026-09-08T12:00:00Z"
"#
        ));
        assert_eq!(hold_note(&unheld), None, "the breaker is not holding");

        let bare: Snapshot = kopiur_api::testutil::from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: { name: nightly-1, namespace: media }
spec: {}
"#,
        );
        assert_eq!(
            hold_note(&bare),
            None,
            "a status the controller has not written yet says nothing either way"
        );
    }

    #[test]
    fn every_replication_kind_has_a_receipt_label() {
        assert_eq!(
            replication_kind_label(ReplicationKind::RepositoryReplication),
            "RepositoryReplication"
        );
        assert_eq!(
            replication_kind_label(ReplicationKind::SnapshotReplication),
            "SnapshotReplication"
        );
    }

    #[test]
    fn a_stated_replication_kind_parses_from_both_spellings() {
        let cases = [
            ("replication", ReplicationKind::RepositoryReplication),
            (
                "RepositoryReplication",
                ReplicationKind::RepositoryReplication,
            ),
            ("snapshot-replication", ReplicationKind::SnapshotReplication),
            ("SnapshotReplication", ReplicationKind::SnapshotReplication),
        ];
        for (raw, want) in cases {
            assert_eq!(replication_kind(raw).expect("parses"), want, "{raw}");
        }
    }

    #[test]
    fn the_replication_table_covers_every_variant_and_refuses_anything_else() {
        for kind in [
            ReplicationKind::RepositoryReplication,
            ReplicationKind::SnapshotReplication,
        ] {
            assert!(
                REPLICATION_KINDS.iter().any(|(_, _, k)| *k == kind),
                "{kind:?} has no accepted spelling"
            );
        }
        assert_eq!(REPLICATION_KINDS.len(), 2);

        for raw in ["", "repository", "snapshot", "repl"] {
            let error = replication_kind(raw).expect_err("must be refused");
            assert_eq!(error.status(), Status::BAD_REQUEST, "{raw:?}");
            assert!(
                error.0.what.contains("omit kind"),
                "the refusal must say detection is the alternative: {}",
                error.0.what
            );
        }
    }

    #[test]
    fn a_replication_run_body_accepts_an_optional_kind() {
        // The field exists so the remediation on an ambiguous name is something
        // the API can actually satisfy; omitting it must stay valid.
        let detected: ReplicationRunBody =
            serde_json::from_value(serde_json::json!({ "namespace": "prod", "name": "nightly" }))
                .expect("kind is optional");
        assert_eq!(detected.kind, None);

        let stated: ReplicationRunBody = serde_json::from_value(serde_json::json!({
            "namespace": "prod",
            "name": "nightly",
            "kind": "SnapshotReplication",
        }))
        .expect("kind is accepted");
        assert_eq!(
            replication_kind(stated.kind.as_deref().expect("set")).expect("parses"),
            ReplicationKind::SnapshotReplication
        );
    }

    #[tokio::test]
    async fn an_unknown_stated_replication_kind_is_refused_before_any_cluster_call() {
        let response = app()
            .oneshot(spa_request(
                "POST",
                "/actions/replication-run",
                r#"{"namespace":"prod","name":"nightly","kind":"Mirror"}"#,
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), Status::BAD_REQUEST);
        assert_eq!(
            problem_of(response).await.r#type,
            "urn:kopiur:problem:invalid-body"
        );
    }

    // --- oversized bodies ---------------------------------------------------

    #[tokio::test]
    async fn a_body_over_the_length_limit_is_a_413_not_a_malformed_request() {
        // axum's default request-body limit is 2 MiB. Answering "malformed"
        // here would send the caller hunting for a typo that is not there.
        let oversized = format!(
            r#"{{"kind":"policy","name":"nightly","namespace":"prod","suspend":true,"pad":"{}"}}"#,
            "x".repeat(3 * 1024 * 1024)
        );
        let response = app()
            .oneshot(spa_request("POST", "/actions/suspend", &oversized))
            .await
            .expect("response");

        assert_eq!(response.status(), Status::PAYLOAD_TOO_LARGE);
        let problem = problem_of(response).await;
        assert_eq!(problem.r#type, "urn:kopiur:problem:body-too-large");
        assert_eq!(problem.status, 413);
        assert!(!problem.fix.is_empty(), "413s carry a remediation too");
        assert_eq!(problem.instance.as_deref(), Some("/actions/suspend"));
    }

    #[test]
    fn a_missing_policy_renders_as_the_same_404_the_apiserver_would_have_sent() {
        let error = ApiError::from(not_found(
            "SnapshotPolicy",
            "snapshotpolicies",
            "prod",
            "nightly",
        ));
        assert_eq!(error.status(), Status::NOT_FOUND);
        assert_eq!(error.0.r#type, "urn:kopiur:problem:not-found");
        assert!(error.0.what.contains("nightly"), "{}", error.0.what);
    }
}
