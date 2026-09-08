//! Browsing a snapshot's contents: listing directories and downloading files out
//! of a running kopia session pod.
//!
//! Sessions are shared with `kubectl kopiur browse` — both go through
//! `kopiur_ops::browse` — so opening one in the UI while one is open on a
//! terminal does not start a second pod.
//!
//! # The split that matters: starting a session is a mutation
//!
//! A browse session is a `Job` that runs a pod holding the repository open. That
//! is a write to the cluster and a real cost, so it is a `POST` behind the CSRF
//! gate — never a side effect of a `GET`. [`tree`] and [`download`] therefore
//! *only ever use* a session that already exists: no live Job means 409
//! `session-required`, and the SPA turns that into a "start browsing" button the
//! human presses. Without that split, a crawler following links, a preloading
//! browser, or a cross-site `<img>` tag could each spin up mover pods.
//!
//! # Everything here runs as the caller
//!
//! Every apiserver call below — resolving the `Snapshot`, listing session `Job`s,
//! creating one, and above all `pods/exec` — is made with the impersonating
//! client for the request's identity. That is load-bearing beyond the usual
//! reason: `pods/exec` in a namespace is effectively read access to that
//! namespace's repository credentials, so it must be the *human's* RBAC that
//! permits it. The UI's own ServiceAccount cannot exec into anything.
//!
//! No handler here reads a `Secret`. The session Job names credential Secrets for
//! the kubelet to mount (`kopiur_ops::browse::resolve::session_creds_secrets`
//! only produces names); the backend CA bundle is a `ConfigMap`.

// `ApiError` wraps an RFC 9457 `Problem`: eight owned strings, ~240 bytes. Every
// handler in the crate returns one, and `IntoResponse` is implemented on the
// bare type — boxing it here would mean unboxing at every route boundary for a
// stack cost that is noise next to an apiserver round-trip. Applied at the
// module root so it covers `download` and `session_pool` too; it belongs in
// `lib.rs` once every `/api/v1` module lands.
#![allow(clippy::result_large_err)]

pub mod download;
pub mod session_pool;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, middleware};

use k8s_openapi::api::batch::v1::Job;

use kopiur_api::common::RepositoryKind;
use kopiur_kopia::{DirEntry, ObjectId, SessionCmd};
use kopiur_ops::browse::resolve::{BrowseTarget, OperatorNamespace, resolve_with};
use kopiur_ops::browse::session::{
    ExecSession, MoverImageSource, TracingProgress, delete_session, find_session_job,
    job_is_terminal,
};
use kopiur_ops::browse::{SnapshotAccess, validate_rel_path, walk_to_dir, walk_to_file};
use kopiur_ops::ctx::{OpsCtx, Scope};
use kopiur_ops::error::OpsError;
use kopiur_ui_model::requests::SessionCreateBody;
use kopiur_ui_model::views::{DirEntryView, DirListing, EntryKind, SessionInfo};

use crate::AppState;
use crate::api::problem::{ApiError, problem};
use crate::auth::csrf::require_same_site_navigation;
use crate::auth::identity::Identity;
use crate::auth::redact::redact_text;
use crate::auth::{CurrentIdentity, mutation_guard};
use crate::config::{FIELD_MANAGER, UiConfig};

use self::session_pool::{SessionKey, session_info};

/// Entries returned when the request names no `limit`.
///
/// A directory in a backup is routinely tens of thousands of entries; a page has
/// to be small enough that the SPA renders it without a virtual list and that
/// the JSON is not megabytes, and large enough that ordinary directories arrive
/// in one request.
pub const DEFAULT_TREE_LIMIT: usize = 500;

/// Largest `limit` the tree endpoint honours. A bigger one is clamped rather
/// than refused: the caller still gets a correct (paginated) answer, and
/// `DirListing.limit` reports what was actually applied.
pub const MAX_TREE_LIMIT: usize = 5000;

/// Shortest session TTL a caller may ask for.
///
/// A session that expires while the pod is still pulling its image is worse than
/// useless — the SPA would start one, watch it die, and start another.
const MIN_SESSION_TTL: Duration = Duration::from_secs(60);

/// The browse routes, mounted by the API router under the paths spelled out
/// here.
///
/// The paths are absolute rather than relative to a nest point on purpose: this
/// router carries the whole `/api/v1/...` prefix, so the routes read the same
/// here as they do in the SPA and in the docs.
///
/// [`mutation_guard`] goes on with `route_layer` rather than `layer`: it then
/// runs only for a request that actually matched a route here, so an unmatched
/// path stays a 404 and a wrong method stays a 405 instead of both being
/// answered with a CSRF refusal that says nothing true. The guard is a no-op for
/// safe methods, so covering the whole router is exactly equivalent to naming
/// `POST`/`DELETE` — and it cannot be forgotten when a route is added.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/snapshots/{namespace}/{name}/session",
            get(get_session).post(create_session).delete(end_session),
        )
        .route(
            "/api/v1/repositories/{kind}/{name}/session",
            delete(end_repository_session),
        )
        .route("/api/v1/snapshots/{namespace}/{name}/tree", get(tree))
        .route("/api/v1/snapshots/{namespace}/{name}/file", get(download))
        .route_layer(middleware::from_fn(mutation_guard))
}

// --- query shapes -----------------------------------------------------------

/// `GET …/tree?path=&offset=&limit=`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TreeQuery {
    /// Directory to list, relative to the snapshot root. Empty is the root.
    #[serde(default)]
    pub path: String,
    /// Index of the first entry to return.
    #[serde(default)]
    pub offset: usize,
    /// Maximum entries to return; clamped to [`MAX_TREE_LIMIT`].
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET …/file?path=`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileQuery {
    /// File to download, relative to the snapshot root.
    pub path: String,
}

/// `DELETE /api/v1/repositories/{kind}/{name}/session?namespace=&sessionNamespace=`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositorySessionQuery {
    /// The repository's own namespace. Omitted for `ClusterRepository`.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Namespace the session Job runs in, when it differs from the repository's
    /// — a session always lives beside the `Snapshot` being browsed, which for a
    /// `ClusterRepository` is any namespace at all.
    #[serde(default)]
    pub session_namespace: Option<String>,
}

// --- handlers ---------------------------------------------------------------

/// `POST /api/v1/snapshots/{namespace}/{name}/session` — start (or join) a
/// browse session for the snapshot's repository.
///
/// The one endpoint here that may create a `Job`. Answers `201` with the session
/// once its pod is Ready, so the SPA's next `GET …/tree` cannot arrive before
/// the pod can serve it.
async fn create_session(
    State(app): State<AppState>,
    CurrentIdentity(identity): CurrentIdentity,
    Path((namespace, name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let requested = parse_session_body(&body)?;
    let ttl = session_ttl(requested.ttl_seconds, app.cfg.session.ttl);

    let ctx = ops_ctx(&app, &identity, &namespace)?;
    let target = resolve_target(&app.cfg, &ctx, &namespace, &name).await?;
    let key = session_key(&target);
    let image = mover_image(&app.cfg);
    let ready_timeout = app.cfg.session.ready_timeout;

    let (live, reused) = app
        .sessions
        .ensure(
            &key,
            || async {
                match find_live_job(&ctx, &target).await? {
                    // A live Job: attach to it. `ExecSession::ensure` takes its
                    // reuse path and creates nothing.
                    Some(_) => open_session(&ctx, &target, ttl, &image, ready_timeout)
                        .await
                        .map(Some),
                    None => Ok(None),
                }
            },
            || open_session(&ctx, &target, ttl, &image, ready_timeout),
        )
        .await?;

    if !reused {
        app.metrics.inc_session_started();
    }

    let mut info = session_info(&live.job, ttl, reused);
    info.pod = Some(live.session.pod.clone());
    Ok((StatusCode::CREATED, Json(info)).into_response())
}

/// `GET /api/v1/snapshots/{namespace}/{name}/session` — report the snapshot's
/// browse session, if one is running.
///
/// Deliberately cheap: it lists `Job`s and stops there. It does not list pods
/// (so `pod` is `None`) and it never waits for readiness, because its job is to
/// tell the SPA whether the browse view can be opened at all — not to make it
/// possible.
async fn get_session(
    State(app): State<AppState>,
    CurrentIdentity(identity): CurrentIdentity,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<SessionInfo>, ApiError> {
    let ctx = ops_ctx(&app, &identity, &namespace)?;
    let target = resolve_target(&app.cfg, &ctx, &namespace, &name).await?;
    match find_live_job(&ctx, &target).await? {
        Some(job) => Ok(Json(session_info(&job, app.cfg.session.ttl, true))),
        None => Err(no_session(404, &namespace, &name)),
    }
}

/// `DELETE /api/v1/snapshots/{namespace}/{name}/session` — stop the snapshot's
/// browse session.
///
/// Idempotent: no session is already the requested state, so it answers `204`
/// rather than 404. A user who clicks "end session" twice, or whose session
/// expired between the page render and the click, gets the outcome they asked
/// for.
async fn end_session(
    State(app): State<AppState>,
    CurrentIdentity(identity): CurrentIdentity,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let ctx = ops_ctx(&app, &identity, &namespace)?;
    let target = resolve_target(&app.cfg, &ctx, &namespace, &name).await?;
    if let Some(job) = find_session_job(
        &ctx,
        &target.namespace,
        target.repo.kind,
        target.repo.namespace.as_deref(),
        &target.repo.name,
    )
    .await?
    {
        delete_session(&ctx, &target.namespace, &kube::ResourceExt::name_any(&job)).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/v1/repositories/{kind}/{name}/session` — stop a session by
/// repository rather than by snapshot.
///
/// The escape hatch for a session whose `Snapshot` has since been deleted: the
/// snapshot-scoped route resolves the repository *through* the Snapshot, so it
/// cannot reach a session the Snapshot's deletion orphaned. Also idempotent.
async fn end_repository_session(
    State(app): State<AppState>,
    CurrentIdentity(identity): CurrentIdentity,
    Path((kind, name)): Path<(String, String)>,
    Query(query): Query<RepositorySessionQuery>,
) -> Result<StatusCode, ApiError> {
    let kind = parse_repository_kind(&kind)?;
    let repo_namespace = match kind {
        RepositoryKind::Repository => Some(require_namespace(query.namespace.as_deref())?),
        // A ClusterRepository has no namespace of its own; the session still
        // lives in some namespace, which the caller must name.
        RepositoryKind::ClusterRepository => None,
    };
    let session_namespace = match (
        query.session_namespace.as_deref(),
        repo_namespace.as_deref(),
    ) {
        (Some(ns), _) => ns.to_string(),
        (None, Some(ns)) => ns.to_string(),
        (None, None) => return Err(missing_session_namespace()),
    };

    let ctx = ops_ctx(&app, &identity, &session_namespace)?;
    if let Some(job) = find_session_job(
        &ctx,
        &session_namespace,
        kind,
        repo_namespace.as_deref(),
        &name,
    )
    .await?
    {
        delete_session(&ctx, &session_namespace, &kube::ResourceExt::name_any(&job)).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/snapshots/{namespace}/{name}/tree?path=&offset=&limit=` — one
/// page of a directory inside the snapshot.
///
/// Never starts a session (see the module docs).
async fn tree(
    State(app): State<AppState>,
    CurrentIdentity(identity): CurrentIdentity,
    Path((namespace, name)): Path<(String, String)>,
    Query(query): Query<TreeQuery>,
) -> Result<Json<DirListing>, ApiError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_TREE_LIMIT)
        .min(MAX_TREE_LIMIT);
    let parts = validate_rel_path(&query.path)?;

    let ctx = ops_ctx(&app, &identity, &namespace)?;
    let target = resolve_target(&app.cfg, &ctx, &namespace, &name).await?;
    let live = require_live_session(&app, &ctx, &target, &namespace, &name).await?;
    let _permit = app.sessions.exec_permit(&identity.user, &app.metrics)?;

    let mut access = CappedAccess::new(live.session, app.cfg.manifest_max_bytes);
    let listing = list_at(
        &mut access,
        &target.kopia_snapshot_id,
        &parts,
        query.offset,
        limit,
    )
    .await
    .map_err(|e| listing_error(e, &access, &query.path))?;

    Ok(Json(DirListing {
        path: query.path,
        entries: listing.entries,
        total: listing.total,
        offset: query.offset,
        limit,
        session: session_info(&live.job, app.cfg.session.ttl, true),
    }))
}

/// `GET /api/v1/snapshots/{namespace}/{name}/file?path=` — stream one file out
/// of the snapshot.
///
/// A browser performs this as a top-level navigation, not a `fetch`, so the SPA
/// cannot attach the `X-Kopiur-Request` header the mutation gate looks for.
/// [`require_same_site_navigation`] is the check that applies to navigations
/// instead. It is a read, so that is the right shape — but it is a read that
/// costs a pod exec, hence the exec permit and the size caps.
async fn download(
    State(app): State<AppState>,
    CurrentIdentity(identity): CurrentIdentity,
    Path((namespace, name)): Path<(String, String)>,
    Query(query): Query<FileQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_same_site_navigation(&headers)?;
    let parts = validate_rel_path(&query.path)?;

    let ctx = ops_ctx(&app, &identity, &namespace)?;
    let target = resolve_target(&app.cfg, &ctx, &namespace, &name).await?;
    let live = require_live_session(&app, &ctx, &target, &namespace, &name).await?;
    let permit = app.sessions.exec_permit(&identity.user, &app.metrics)?;

    let mut access = CappedAccess::new(live.session, app.cfg.manifest_max_bytes);
    let root = access.snapshot_root(&target.kopia_snapshot_id).await?;
    let (oid, entry) = walk_to_file(&mut access, &root, &parts, &query.path).await?;
    let size = download_size(&entry, &query.path, app.cfg.download_max_bytes)?;

    let body = download::stream_file(
        access.into_session(),
        oid,
        size,
        permit,
        app.metrics.clone(),
    );
    Ok(download_response(&entry.name, size, body))
}

// --- the pure core ----------------------------------------------------------

/// One page of a directory: the entries plus how many the directory holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// The entries in this page, in kopia's manifest order.
    pub entries: Vec<DirEntryView>,
    /// How many entries the directory holds in total.
    pub total: usize,
}

/// Walk to `parts` and return one page of it.
///
/// Generic over [`SnapshotAccess`] so the whole listing path — root resolution,
/// the walk, the entry mapping and the pagination arithmetic — is tested against
/// a fake, with no cluster and no session pod.
///
/// An `offset` past the end is an empty page, not an error: a directory shrinks
/// between snapshots and between clicks, and a browser that has paged to the end
/// of yesterday's listing should see "nothing here", not a failure.
pub async fn list_at<A: SnapshotAccess + ?Sized>(
    access: &mut A,
    kopia_snapshot_id: &str,
    parts: &[String],
    offset: usize,
    limit: usize,
) -> Result<Page, A::Error> {
    let root = access.snapshot_root(kopia_snapshot_id).await?;
    let (_oid, manifest) = walk_to_dir(access, &root, parts).await?;
    let total = manifest.entries.len();
    let entries = manifest
        .entries
        .iter()
        .skip(offset)
        .take(limit)
        .map(entry_view)
        .collect();
    Ok(Page { entries, total })
}

/// Map one kopia manifest entry onto the wire type.
///
/// The `match` is exhaustive over the entry types kopia documents plus an
/// `Other { raw }` catch-all that carries the string through verbatim — so a
/// socket, a device node, or a type a future kopia introduces renders as
/// something the SPA can name, instead of being silently shown as a file the
/// user then tries to download.
pub fn entry_view(entry: &DirEntry) -> DirEntryView {
    let kind = match entry.entry_type.as_str() {
        "f" => EntryKind::File,
        "d" => EntryKind::Dir,
        "s" => EntryKind::Symlink,
        raw => EntryKind::Other {
            raw: raw.to_string(),
        },
    };
    DirEntryView {
        name: entry.name.clone(),
        kind,
        // A directory's own `size` is absent; its subtree total lives in `summ`.
        size: entry
            .size
            .or_else(|| entry.summ.as_ref().and_then(|s| s.size)),
        mtime: entry.mtime.clone(),
        mode: entry.mode.clone(),
    }
}

/// The `Content-Disposition` for a downloaded file.
///
/// RFC 5987 `filename*=UTF-8''…` only, with no plain `filename=` fallback: every
/// browser that matters has understood the extended form for a decade, and the
/// plain form is exactly where header injection and path traversal live. The
/// value is derived, never echoed —
///
/// * only the **basename** survives, so `../../etc/passwd` saves as `passwd`;
/// * control characters (CR/LF included) are dropped, so a crafted filename
///   cannot split the header;
/// * `"` is dropped, so it cannot end the quoted form some clients still parse;
/// * everything outside RFC 5987's `attr-char` set is percent-encoded, which
///   covers spaces, `%`, `;`, and every non-ASCII byte.
///
/// A name that is empty once sanitized becomes `download`, because a browser
/// given an empty filename invents one from the URL — which is the snapshot's
/// path.
pub fn content_disposition(name: &str) -> HeaderValue {
    let base = name.rsplit('/').next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .collect();
    let cleaned = cleaned.trim();
    let encoded = if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "download".to_string()
    } else {
        percent_encode_attr(cleaned)
    };
    let value = format!("attachment; filename*=UTF-8''{encoded}");
    // Every byte is now ASCII and non-control, so this cannot fail; degrade to a
    // bare `attachment` rather than panic inside a handler if it somehow does.
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// Percent-encode everything outside RFC 5987's `attr-char`.
///
/// `attr-char` is `ALPHA / DIGIT / "!" / "#" / "$" / "&" / "+" / "-" / "." /
/// "^" / "_" / "`" / "|" / "~"`. Deliberately hand-rolled rather than pulled
/// from a crate: the set is five lines, and the whole point of this function is
/// that its behaviour is legible next to the test that pins it.
fn percent_encode_attr(s: &str) -> String {
    const SAFE: &[u8] = b"!#$&+-.^_`|~";
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        if byte.is_ascii_alphanumeric() || SAFE.contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The response headers a download carries, beside the streamed body.
///
/// `Content-Length` is the size the snapshot recorded, committed before a byte
/// is read — see [`download`] for why that is a contract the body is then held
/// to. `nosniff` and the octet-stream type keep a `.html` in a backup from being
/// rendered as a page on the UI's own origin; `no-store` keeps a file a caller
/// was allowed to read once out of a shared cache.
fn download_response(name: &str, size: u64, body: Body) -> Response {
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            ),
            (header::CONTENT_DISPOSITION, content_disposition(name)),
            (
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&size.to_string())
                    .unwrap_or_else(|_| HeaderValue::from_static("0")),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response()
}

/// The byte count a download will promise, or the reason there is none.
///
/// Both refusals happen *before* any bytes move, which is the only useful time
/// for them: a 413 discovered halfway through is a truncated file with a 200.
pub fn download_size(entry: &DirEntry, path: &str, max: u64) -> Result<u64, ApiError> {
    let Some(size) = entry.size.filter(|s| *s >= 0) else {
        return Err(problem(
            422,
            "download-size-unknown",
            format!("The snapshot records no size for {path}."),
            "kopiur-ui commits a Content-Length before it streams, so that a truncated \
             transfer is visible to the browser rather than silently saved. An entry with no \
             recorded size cannot be served that way.",
            format!(
                "read it with `kubectl kopiur cat {path}`, which streams without a declared \
                 length"
            ),
        ));
    };
    let size = size as u64;
    if size > max {
        return Err(problem(
            413,
            "download-too-large",
            format!("{path} is {size} bytes, above this deployment's download limit of {max}."),
            "A browser download is buffered by the browser and streamed through kopiur-ui's \
             own process, so the limit bounds what one click can cost the UI pod and the \
             session pod.",
            format!(
                "restore the file instead — `kubectl kopiur download {path}` streams straight \
                 to disk — or raise KOPIUR_UI_MAX_DOWNLOAD_BYTES"
            ),
        ));
    }
    Ok(size)
}

// --- the session transport --------------------------------------------------

/// A [`SnapshotAccess`] over a session pod whose JSON captures are capped.
///
/// The cap is the whole reason this exists rather than using `ExecSession`'s own
/// `SnapshotAccess` impl: `exec_capture` buffers a manifest with no bound, and a
/// directory with a few million entries would be answered by the UI pod being
/// OOM-killed. Here it is a 422 that names the limit.
///
/// File *content* deliberately does not go through here — it streams
/// ([`download::stream_file`]) — so the cap applies to manifests only.
pub struct CappedAccess {
    session: ExecSession,
    manifest_cap: u64,
    /// Set when a capture was cut off, so the handler can tell "this directory
    /// is too big" from "the exec failed".
    exceeded: bool,
}

impl CappedAccess {
    /// Wrap a session, capping every JSON capture at `manifest_cap` bytes.
    pub fn new(session: ExecSession, manifest_cap: u64) -> Self {
        Self {
            session,
            manifest_cap,
            exceeded: false,
        }
    }

    /// Whether a capture hit the cap.
    pub fn exceeded(&self) -> bool {
        self.exceeded
    }

    /// Give the session back, for the streaming download that follows a walk.
    pub fn into_session(self) -> ExecSession {
        self.session
    }

    /// Run one session command and buffer its stdout, refusing to grow past the
    /// cap.
    async fn capture_capped(&mut self, cmd: SessionCmd) -> Result<Vec<u8>, OpsError> {
        let mut sink = CappedSink::new(Vec::new(), self.manifest_cap);
        let result = self.session.exec_stream(cmd, &mut sink).await;
        if sink.exceeded() {
            self.exceeded = true;
        }
        result?;
        Ok(sink.into_inner())
    }
}

impl SnapshotAccess for CappedAccess {
    type Error = OpsError;

    async fn snapshot_root(&mut self, kopia_snapshot_id: &str) -> Result<ObjectId, OpsError> {
        let out = self.capture_capped(SessionCmd::SnapshotListJson).await?;
        kopiur_ops::browse::root_oid_from_list(&out, kopia_snapshot_id)
    }

    async fn list_dir(&mut self, oid: &ObjectId) -> Result<kopiur_kopia::DirManifest, OpsError> {
        let out = self
            .capture_capped(SessionCmd::ShowObject { oid: oid.clone() })
            .await?;
        kopiur_ops::browse::parse_dir_manifest(&out, oid)
    }

    async fn read_file(
        &mut self,
        oid: &ObjectId,
        sink: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
    ) -> Result<u64, OpsError> {
        self.session
            .exec_stream(SessionCmd::ShowObject { oid: oid.clone() }, sink)
            .await
    }
}

/// An [`AsyncWrite`](tokio::io::AsyncWrite) that refuses to accept more than
/// `cap` bytes.
///
/// Unlike [`download::ExactSink`], which enforces a size the snapshot promised,
/// this enforces a size the *operator* chose: it is a memory bound on buffering
/// untrusted kopia output, not a correctness check on a transfer.
pub struct CappedSink<W> {
    inner: W,
    cap: u64,
    written: u64,
    exceeded: bool,
}

impl<W> CappedSink<W> {
    /// Wrap `inner`, accepting at most `cap` bytes.
    pub fn new(inner: W, cap: u64) -> Self {
        Self {
            inner,
            cap,
            written: 0,
            exceeded: false,
        }
    }

    /// Whether a write was refused for exceeding the cap.
    pub fn exceeded(&self) -> bool {
        self.exceeded
    }

    /// Bytes accepted so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Unwrap the buffer that was written into.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for CappedSink<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.written.saturating_add(buf.len() as u64) > self.cap {
            self.exceeded = true;
            return std::task::Poll::Ready(Err(std::io::Error::other(format!(
                "the kopia output exceeded KOPIUR_UI_MAX_MANIFEST_BYTES ({} bytes)",
                self.cap
            ))));
        }
        let result = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &result {
            self.written += *n as u64;
        }
        result
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// --- shared plumbing --------------------------------------------------------

/// A live session: the Ready pod to exec into, and the Job it belongs to (which
/// is what carries the creation time an expiry is computed from).
struct LiveSession {
    session: ExecSession,
    job: Job,
}

/// The ops context for one request, speaking as the caller.
///
/// Built per request rather than held in `AppState`, because the client it
/// carries is the caller's impersonating client — a shared one would be a
/// different person's.
fn ops_ctx(app: &AppState, identity: &Identity, namespace: &str) -> Result<OpsCtx, ApiError> {
    let client = app.auth.clients.client_for(identity).map_err(|e| {
        problem(
            500,
            "internal",
            "kopiur-ui could not build a Kubernetes client for this request.",
            format!("Constructing the impersonating client failed: {e}"),
            "check kopiur-ui's own ServiceAccount and in-cluster configuration, and report \
             this at https://github.com/home-operations/kopiur/issues with the UI's logs",
        )
    })?;
    Ok(OpsCtx {
        client,
        namespace: namespace.to_string(),
        scope: Scope::Namespace(namespace.to_string()),
        field_manager: FIELD_MANAGER.to_string(),
    })
}

/// Resolve the `Snapshot` to its repository, telling ops the operator namespace
/// when the chart told us.
///
/// The alternative — `DiscoverFromControllerDeployment` — is a cluster-wide
/// `Deployment` LIST, which is both slower and something the impersonated human
/// very likely cannot do (the UI's roles deliberately drop `apps/deployments`).
/// So a deployment that sets `KOPIUR_NAMESPACE` never asks.
async fn resolve_target(
    cfg: &UiConfig,
    ctx: &OpsCtx,
    namespace: &str,
    name: &str,
) -> Result<BrowseTarget, ApiError> {
    let operator_namespace = match &cfg.operator_namespace {
        Some(ns) => OperatorNamespace::Known(ns.clone()),
        None => OperatorNamespace::DiscoverFromControllerDeployment,
    };
    Ok(resolve_with(ctx, namespace, name, &operator_namespace).await?)
}

/// Which mover image session pods run: the chart's value when it stamped one,
/// otherwise the fail-closed lookup on the controller Deployment.
fn mover_image(cfg: &UiConfig) -> MoverImageSource {
    match &cfg.mover_image {
        Some(image) => MoverImageSource::Fixed(image.clone()),
        None => MoverImageSource::DiscoverFromControllerDeployment,
    }
}

/// The session key for a target: one warm pod per repository per namespace.
fn session_key(target: &BrowseTarget) -> SessionKey {
    SessionKey {
        namespace: target.namespace.clone(),
        kind: target.repo.kind,
        repo_namespace: target.repo.namespace.clone(),
        repo_name: target.repo.name.clone(),
    }
}

/// The target's session `Job`, if one exists and has not finished.
///
/// A terminal Job is not a session: its pod is gone, so exec'ing into it would
/// fail. Reporting it as one would show the SPA a browse view that answers every
/// click with an error.
async fn find_live_job(ctx: &OpsCtx, target: &BrowseTarget) -> Result<Option<Job>, OpsError> {
    Ok(find_session_job(
        ctx,
        &target.namespace,
        target.repo.kind,
        target.repo.namespace.as_deref(),
        &target.repo.name,
    )
    .await?
    .filter(|job| !job_is_terminal(job)))
}

/// Attach to (or, on the create path, start) the session and wait for its pod.
///
/// The wait is bounded by `KOPIUR_UI_SESSION_READY_TIMEOUT` rather than by ops'
/// own internal budget, because a browser request cannot hang for five minutes:
/// a `504` the SPA can retry is a better answer than a connection held open past
/// every proxy's own timeout.
async fn open_session(
    ctx: &OpsCtx,
    target: &BrowseTarget,
    ttl: Duration,
    image: &MoverImageSource,
    ready_timeout: Duration,
) -> Result<LiveSession, ApiError> {
    let session = tokio::time::timeout(
        ready_timeout,
        ExecSession::ensure(ctx, target, ttl, image, &TracingProgress),
    )
    .await
    .map_err(|_| session_ready_timeout(ready_timeout))?
    .map_err(session_error)?;

    // Re-read the Job for its creation timestamp; `ExecSession` carries the
    // names, not the object.
    let job = find_session_job(
        ctx,
        &target.namespace,
        target.repo.kind,
        target.repo.namespace.as_deref(),
        &target.repo.name,
    )
    .await?
    .ok_or_else(|| {
        problem(
            502,
            "upstream",
            "The browse session pod became ready but its Job vanished.",
            "Something deleted the Job between kopiur-ui starting it and reading it back.",
            "retry; if it keeps happening, look for a controller or policy engine deleting \
             kopiur-browse-* Jobs",
        )
    })?;
    Ok(LiveSession { session, job })
}

/// The session a read must use, or the 409 that tells the SPA to start one.
///
/// The existence check and the attach are two calls, so a session deleted in
/// between would be re-created by the attach. That window is one apiserver
/// round-trip wide and needs a human to delete a session inside it; the check is
/// what makes a *routine* `GET` — a crawler, a preload, a link in a chat — unable
/// to create anything.
async fn require_live_session(
    app: &AppState,
    ctx: &OpsCtx,
    target: &BrowseTarget,
    namespace: &str,
    name: &str,
) -> Result<LiveSession, ApiError> {
    if find_live_job(ctx, target).await?.is_none() {
        return Err(no_session(409, namespace, name));
    }
    open_session(
        ctx,
        target,
        app.cfg.session.ttl,
        &mover_image(&app.cfg),
        app.cfg.session.ready_timeout,
    )
    .await
}

/// The TTL a session is started with: what the caller asked for, clamped into
/// `[MIN_SESSION_TTL, configured maximum]`.
///
/// Clamped rather than refused, because the value is a hint about how long the
/// user expects to browse, not an assertion the server has to honour exactly —
/// and because the maximum is the operator's cost decision, which no request
/// gets to raise.
fn session_ttl(requested: Option<u64>, max: Duration) -> Duration {
    match requested {
        Some(seconds) => Duration::from_secs(seconds).clamp(MIN_SESSION_TTL.min(max), max),
        None => max,
    }
}

/// Parse the optional `POST …/session` body.
///
/// An empty body is the common case (the SPA sends none), so it is the default
/// rather than an error; anything present must be the exact documented shape,
/// since `SessionCreateBody` denies unknown fields and a silently-ignored
/// `ttlSeconds` typo would be a session with the wrong lifetime.
fn parse_session_body(body: &[u8]) -> Result<SessionCreateBody, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(SessionCreateBody { ttl_seconds: None });
    }
    serde_json::from_slice(body).map_err(|e| {
        problem(
            400,
            "invalid",
            "The session request body is not the shape this endpoint accepts.",
            format!("Parsing it failed: {e}"),
            "send either no body at all or {\"ttlSeconds\": <number>}",
        )
    })
}

/// `Repository` or `ClusterRepository` from the path segment, case-insensitively.
fn parse_repository_kind(kind: &str) -> Result<RepositoryKind, ApiError> {
    if kind.eq_ignore_ascii_case("repository") {
        Ok(RepositoryKind::Repository)
    } else if kind.eq_ignore_ascii_case("clusterrepository") {
        Ok(RepositoryKind::ClusterRepository)
    } else {
        Err(problem(
            400,
            "invalid",
            format!("{kind:?} is not a repository kind."),
            "Kopiur has two: the namespaced Repository and the cluster-scoped \
             ClusterRepository.",
            "use Repository or ClusterRepository in the URL",
        ))
    }
}

/// A namespaced `Repository` needs its namespace named.
fn require_namespace(namespace: Option<&str>) -> Result<String, ApiError> {
    match namespace.filter(|ns| !ns.is_empty()) {
        Some(ns) => Ok(ns.to_string()),
        None => Err(problem(
            400,
            "invalid",
            "The request names a Repository but no namespace.",
            "Repository is namespaced, so its name alone does not identify one.",
            "add ?namespace=<the repository's namespace> to the URL",
        )),
    }
}

/// A `ClusterRepository` has no namespace of its own, so the session's must be
/// named explicitly.
fn missing_session_namespace() -> ApiError {
    problem(
        400,
        "invalid",
        "The request names a ClusterRepository but no session namespace.",
        "A ClusterRepository is cluster-scoped, but its browse session Job runs beside the \
         Snapshot being browsed — so the namespace to look in cannot be derived from the \
         repository.",
        "add ?sessionNamespace=<the snapshot's namespace> to the URL",
    )
}

/// The 409 (or 404) that says a browse session has to be started first.
///
/// One function for both statuses so the `type` URN, the explanation, and the
/// remediation cannot drift between "you asked whether there is one" and "you
/// tried to use one".
fn no_session(status: u16, namespace: &str, name: &str) -> ApiError {
    problem(
        status,
        "session-required",
        format!("No browse session is running for {namespace}/{name}."),
        "Reading a snapshot's files needs a mover pod holding the repository open. \
         kopiur-ui never starts one from a GET — that is a write to the cluster and a real \
         cost, so it takes a deliberate action.",
        "start a browse session first (the browse view's \"start session\" button, or POST \
         to this snapshot's /session endpoint)",
    )
}

/// A readiness wait that ran out of budget.
fn session_ready_timeout(budget: Duration) -> ApiError {
    problem(
        504,
        "timeout",
        format!(
            "The browse session pod was not ready within {}s.",
            budget.as_secs()
        ),
        "The pod may still be pulling the mover image or connecting to the repository. \
         Waiting stopped; the session did not.",
        "retry in a moment — a warm session answers immediately. If it never becomes ready, \
         check the kopiur-browse-* pod's events and logs, and raise \
         KOPIUR_UI_SESSION_READY_TIMEOUT if image pulls are slow here",
    )
}

/// Turn an ops failure into an API error, redacting a pod log tail first.
///
/// [`OpsError::SessionPodFailed`] carries the last lines of a failed session
/// pod's log, which is exactly the diagnostic a user needs — and exactly where a
/// mover that logged its environment would put credentials. It is the only
/// `OpsError` whose payload is untrusted text rather than kopiur's own prose, so
/// it is the only one rewritten here.
fn session_error(error: OpsError) -> ApiError {
    match error {
        OpsError::SessionPodFailed {
            job,
            namespace,
            detail,
        } => ApiError::from(OpsError::SessionPodFailed {
            job,
            namespace,
            detail: redact_text(&detail),
        }),
        other => ApiError::from(other),
    }
}

/// Turn a listing failure into an API error, naming the cap when that is what
/// was hit.
fn listing_error(error: OpsError, access: &CappedAccess, path: &str) -> ApiError {
    if access.exceeded() {
        return problem(
            422,
            "directory-too-large",
            format!(
                "The listing for {:?} is larger than this deployment will buffer ({} bytes).",
                if path.is_empty() { "/" } else { path },
                access.manifest_cap
            ),
            "kopiur-ui reads a directory by buffering kopia's whole JSON manifest for it, so \
             a directory with millions of entries would be answered by the UI pod running out \
             of memory. The limit turns that into this message.",
            "list it with `kubectl kopiur ls`, which streams instead of buffering, or raise \
             KOPIUR_UI_MAX_MANIFEST_BYTES",
        );
    }
    session_error(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use kopiur_kopia::DirManifest;
    use kopiur_ops::browse::parse_oid;
    use tokio::io::AsyncWriteExt as _;

    // --- entry mapping ------------------------------------------------------

    fn entry(value: serde_json::Value) -> DirEntry {
        serde_json::from_value(value).expect("entry fixture")
    }

    #[test]
    fn every_kopia_entry_type_maps_to_a_kind_the_spa_can_render() {
        let file = entry(serde_json::json!({
            "name": "a.txt", "type": "f", "obj": "kfile-a",
            "size": 16, "mtime": "2026-06-11T01:02:03Z", "mode": "0644"
        }));
        let view = entry_view(&file);
        assert_eq!(view.kind, EntryKind::File);
        assert_eq!(view.name, "a.txt");
        assert_eq!(view.size, Some(16));
        assert_eq!(view.mtime.as_deref(), Some("2026-06-11T01:02:03Z"));
        assert_eq!(view.mode.as_deref(), Some("0644"));

        // A directory carries no `size` of its own; the subtree total is in
        // `summ`, and showing it is what makes a listing useful.
        let dir = entry(serde_json::json!({
            "name": "sub", "type": "d", "obj": "kdir-sub",
            "summ": { "size": 4096, "files": 3, "dirs": 1 }
        }));
        let view = entry_view(&dir);
        assert_eq!(view.kind, EntryKind::Dir);
        assert_eq!(view.size, Some(4096));

        let link = entry(serde_json::json!({ "name": "l", "type": "s", "obj": "k1" }));
        assert_eq!(entry_view(&link).kind, EntryKind::Symlink);

        // Anything else keeps kopia's own letter, so a socket or a device node
        // is nameable rather than silently shown as a downloadable file.
        for raw in ["S", "b", "c", "p", "future-type", ""] {
            let other = entry(serde_json::json!({ "name": "x", "type": raw, "obj": "k1" }));
            assert_eq!(
                entry_view(&other).kind,
                EntryKind::Other {
                    raw: raw.to_string()
                },
                "type {raw:?}"
            );
        }
    }

    // --- content disposition ------------------------------------------------

    fn disposition(name: &str) -> String {
        content_disposition(name)
            .to_str()
            .expect("ascii header")
            .to_string()
    }

    #[test]
    fn a_download_filename_is_derived_never_echoed() {
        assert_eq!(
            disposition("a b.txt"),
            "attachment; filename*=UTF-8''a%20b.txt",
            "a space must be encoded, not quoted"
        );

        // Traversal: only the basename ever reaches the browser.
        assert_eq!(disposition("../evil"), "attachment; filename*=UTF-8''evil");
        assert_eq!(
            disposition("../../etc/passwd"),
            "attachment; filename*=UTF-8''passwd"
        );

        // A quote cannot end the quoted form some clients still parse.
        assert_eq!(disposition("q\"uote"), "attachment; filename*=UTF-8''quote");

        // Non-ASCII is percent-encoded per byte of its UTF-8 encoding.
        assert_eq!(
            disposition("ü.txt"),
            "attachment; filename*=UTF-8''%C3%BC.txt"
        );
        assert_eq!(
            disposition("日本語.bin"),
            "attachment; filename*=UTF-8''%E6%97%A5%E6%9C%AC%E8%AA%9E.bin"
        );

        // Header injection: CR/LF (and every other control char) are dropped, so
        // the value stays one header.
        let injected = disposition("a\r\nSet-Cookie: x=1\u{0}.txt");
        assert!(
            !injected.contains('\r') && !injected.contains('\n'),
            "{injected}"
        );
        assert_eq!(
            injected,
            "attachment; filename*=UTF-8''aSet-Cookie%3A%20x%3D1.txt"
        );

        // `;` and `%` cannot introduce another parameter or a bogus escape.
        assert_eq!(
            disposition("a;b%2e.txt"),
            "attachment; filename*=UTF-8''a%3Bb%252e.txt"
        );

        // Nothing usable left → a name, never an empty one the browser would
        // replace with the request path.
        for empty in ["", "   ", "\r\n", "..", ".", "dir/"] {
            assert_eq!(
                disposition(empty),
                "attachment; filename*=UTF-8''download",
                "{empty:?}"
            );
        }
    }

    // --- capped sink --------------------------------------------------------

    #[tokio::test]
    async fn the_capped_sink_accepts_up_to_the_cap_and_then_refuses() {
        let mut sink = CappedSink::new(Vec::new(), 8);
        sink.write_all(b"12345678").await.expect("exactly the cap");
        assert_eq!(sink.written(), 8);
        assert!(!sink.exceeded());

        let error = sink.write_all(b"9").await.expect_err("one byte past");
        assert!(sink.exceeded());
        assert_eq!(sink.written(), 8, "nothing past the cap is buffered");
        assert!(
            error.to_string().contains("KOPIUR_UI_MAX_MANIFEST_BYTES"),
            "the message must name the knob: {error}"
        );
        assert_eq!(sink.into_inner(), b"12345678");
    }

    #[tokio::test]
    async fn a_single_oversized_write_is_refused_whole() {
        let mut sink = CappedSink::new(Vec::new(), 4);
        sink.write_all(b"0123456789")
            .await
            .expect_err("larger than the cap");
        assert!(sink.exceeded());
        assert_eq!(sink.written(), 0, "a half-buffered manifest is not JSON");
    }

    // --- the listing core, against a fake session ---------------------------

    /// The shape `crates/ops/src/browse/mod.rs` tests against: a map of
    /// dir-oid → manifest. Enough to drive `list_at` with no cluster.
    struct FakeAccess {
        root: String,
        dirs: BTreeMap<String, DirManifest>,
        list_calls: usize,
    }

    impl SnapshotAccess for FakeAccess {
        type Error = OpsError;

        async fn snapshot_root(&mut self, _id: &str) -> Result<ObjectId, OpsError> {
            parse_oid(&self.root)
        }

        async fn list_dir(&mut self, oid: &ObjectId) -> Result<DirManifest, OpsError> {
            self.list_calls += 1;
            self.dirs
                .get(oid.as_str())
                .cloned()
                .ok_or_else(|| OpsError::UnexpectedKopiaOutput {
                    what: format!("dir {}", oid.as_str()),
                    detail: "fake: unknown dir oid".into(),
                })
        }

        async fn read_file(
            &mut self,
            _oid: &ObjectId,
            _sink: &mut (dyn tokio::io::AsyncWrite + Unpin + Send),
        ) -> Result<u64, OpsError> {
            unreachable!("the listing core never reads file content")
        }
    }

    fn manifest(entries: serde_json::Value) -> DirManifest {
        serde_json::from_value(serde_json::json!({
            "stream": "kopia:directory", "entries": entries
        }))
        .expect("manifest fixture")
    }

    /// root/ { a.txt, sub/ { e0.. e9 } }
    fn fake() -> FakeAccess {
        let numbered: Vec<serde_json::Value> = (0..10)
            .map(|i| {
                serde_json::json!({
                    "name": format!("e{i}"), "type": "f",
                    "obj": format!("kfile-{i}"), "size": i
                })
            })
            .collect();
        FakeAccess {
            root: "kroot".into(),
            dirs: BTreeMap::from([
                (
                    "kroot".to_string(),
                    manifest(serde_json::json!([
                        { "name": "a.txt", "type": "f", "obj": "kfile-a", "size": 16 },
                        { "name": "sub", "type": "d", "obj": "kdir-sub" }
                    ])),
                ),
                ("kdir-sub".to_string(), manifest(numbered.into())),
            ]),
            list_calls: 0,
        }
    }

    #[tokio::test]
    async fn the_root_lists_without_a_path() {
        let mut access = fake();
        let page = list_at(&mut access, "ksnap", &[], 0, DEFAULT_TREE_LIMIT)
            .await
            .expect("root listing");
        assert_eq!(page.total, 2);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[0].name, "a.txt");
        assert_eq!(page.entries[1].kind, EntryKind::Dir);
    }

    #[tokio::test]
    async fn pagination_reports_the_whole_directory_and_returns_one_window() {
        let mut access = fake();
        let parts = validate_rel_path("sub").expect("valid path");

        let page = list_at(&mut access, "ksnap", &parts, 0, 3)
            .await
            .expect("first page");
        assert_eq!(page.total, 10, "total is the directory, not the page");
        assert_eq!(
            page.entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["e0", "e1", "e2"]
        );

        let page = list_at(&mut access, "ksnap", &parts, 8, 3)
            .await
            .expect("last, partial page");
        assert_eq!(page.total, 10);
        assert_eq!(
            page.entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["e8", "e9"],
            "a window that runs off the end is short, not an error"
        );

        // Past the end is an empty page: a directory shrinks between snapshots
        // and a stale link must not 500.
        let page = list_at(&mut access, "ksnap", &parts, 999, 3)
            .await
            .expect("past the end");
        assert_eq!(page.total, 10);
        assert!(page.entries.is_empty());

        // A zero limit is a legal (empty) window whose `total` still tells the
        // SPA how many pages there are.
        let page = list_at(&mut access, "ksnap", &parts, 0, 0)
            .await
            .expect("zero limit");
        assert_eq!((page.total, page.entries.len()), (10, 0));
    }

    #[tokio::test]
    async fn a_missing_path_is_the_ops_not_found_and_a_file_is_not_a_directory() {
        let mut access = fake();
        let parts = validate_rel_path("nope").expect("valid path");
        assert!(matches!(
            list_at(&mut access, "ksnap", &parts, 0, 10).await,
            Err(OpsError::PathNotFound { .. })
        ));

        let parts = validate_rel_path("a.txt").expect("valid path");
        assert!(matches!(
            list_at(&mut access, "ksnap", &parts, 0, 10).await,
            Err(OpsError::NotADirectory { .. })
        ));
    }

    #[tokio::test]
    async fn a_path_that_escapes_the_snapshot_never_reaches_the_session() {
        // The gate is `validate_rel_path`, which the handler calls before it
        // touches the cluster — asserted here so a refactor that moved the walk
        // ahead of it would fail.
        for escape in ["/etc/passwd", "a/../../b", ".."] {
            assert!(
                matches!(validate_rel_path(escape), Err(OpsError::InvalidPath { .. })),
                "{escape:?} must be refused"
            );
        }
        let access = fake();
        assert_eq!(access.list_calls, 0);
    }

    // --- download sizing ----------------------------------------------------

    #[test]
    fn a_download_is_refused_before_it_starts_when_it_cannot_be_promised() {
        let sized = entry(serde_json::json!({
            "name": "a.txt", "type": "f", "obj": "k1", "size": 16
        }));
        assert_eq!(download_size(&sized, "a.txt", 1024).expect("in budget"), 16);

        // Above the cap: a 413 naming both numbers, before a byte moves.
        let error = download_size(&sized, "a.txt", 8).expect_err("over the cap");
        assert_eq!(error.status(), 413);
        assert_eq!(error.0.r#type, "urn:kopiur:problem:download-too-large");
        assert!(
            error.0.what.contains("16") && error.0.what.contains('8'),
            "{:?}",
            error.0.what
        );
        assert!(
            error.0.fix.contains("KOPIUR_UI_MAX_DOWNLOAD_BYTES"),
            "{:?}",
            error.0.fix
        );

        // No recorded size: a Content-Length cannot be promised, so it is a 422
        // that points at the streaming CLI rather than a silent truncation.
        let no_size = entry(serde_json::json!({ "name": "s", "type": "f", "obj": "k1" }));
        let error = download_size(&no_size, "s", 1024).expect_err("no size");
        assert_eq!(error.status(), 422);
        assert_eq!(error.0.r#type, "urn:kopiur:problem:download-size-unknown");
        assert!(
            error.0.fix.contains("kubectl kopiur cat"),
            "{:?}",
            error.0.fix
        );

        // A negative size is nonsense, not a zero-byte file.
        let negative = entry(serde_json::json!({
            "name": "s", "type": "f", "obj": "k1", "size": -1
        }));
        assert_eq!(
            download_size(&negative, "s", 1024)
                .expect_err("negative")
                .status(),
            422
        );

        // Exactly at the cap is allowed; the check is `>`, not `>=`.
        let exact = entry(serde_json::json!({
            "name": "a", "type": "f", "obj": "k1", "size": 8
        }));
        assert_eq!(download_size(&exact, "a", 8).expect("exactly the cap"), 8);
    }

    #[test]
    fn the_download_response_commits_the_length_and_refuses_sniffing() {
        let response = download_response("a b.txt", 16, Body::empty());
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        assert_eq!(headers.get(header::CONTENT_LENGTH).unwrap(), "16");
        assert_eq!(
            headers.get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment; filename*=UTF-8''a%20b.txt"
        );
        assert_eq!(
            headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    }

    // --- request plumbing ---------------------------------------------------

    #[test]
    fn a_requested_ttl_is_clamped_into_the_operators_range() {
        let max = Duration::from_secs(900);
        assert_eq!(session_ttl(None, max), max, "no request means the maximum");
        assert_eq!(session_ttl(Some(300), max), Duration::from_secs(300));
        assert_eq!(
            session_ttl(Some(86_400), max),
            max,
            "a request never raises the operator's ceiling"
        );
        assert_eq!(
            session_ttl(Some(0), max),
            MIN_SESSION_TTL,
            "a session that expires before its pod is ready helps nobody"
        );
        // A deployment whose ceiling is below the floor still gets its ceiling.
        let tiny = Duration::from_secs(10);
        assert_eq!(session_ttl(Some(0), tiny), tiny);
        assert_eq!(session_ttl(Some(600), tiny), tiny);
    }

    #[test]
    fn the_session_body_is_optional_but_strict_when_present() {
        assert_eq!(parse_session_body(b"").expect("empty").ttl_seconds, None);
        assert_eq!(
            parse_session_body(b"  \n").expect("whitespace").ttl_seconds,
            None
        );
        assert_eq!(
            parse_session_body(br#"{"ttlSeconds": 300}"#)
                .expect("the documented shape")
                .ttl_seconds,
            Some(300)
        );
        assert_eq!(
            parse_session_body(b"{}")
                .expect("an empty object")
                .ttl_seconds,
            None
        );

        // A typo must not be silently ignored — it would be a session with the
        // wrong lifetime and no way for the user to tell.
        let error = parse_session_body(br#"{"ttl_seconds": 300}"#).expect_err("snake_case typo");
        assert_eq!(error.status(), 400);
        let error = parse_session_body(b"not json").expect_err("garbage");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn the_repository_kind_in_a_url_is_one_of_two_names() {
        assert_eq!(
            parse_repository_kind("Repository").expect("kind"),
            RepositoryKind::Repository
        );
        assert_eq!(
            parse_repository_kind("clusterrepository").expect("kind"),
            RepositoryKind::ClusterRepository
        );
        let error = parse_repository_kind("Secret").expect_err("not a repository kind");
        assert_eq!(error.status(), 400);
        assert!(
            error.0.fix.contains("ClusterRepository"),
            "{:?}",
            error.0.fix
        );
    }

    #[test]
    fn a_namespaced_repository_needs_its_namespace() {
        assert_eq!(require_namespace(Some("media")).expect("named"), "media");
        assert_eq!(require_namespace(None).expect_err("unnamed").status(), 400);
        assert_eq!(
            require_namespace(Some(""))
                .expect_err("empty is unset")
                .status(),
            400
        );
        assert_eq!(missing_session_namespace().status(), 400);
    }

    #[test]
    fn the_session_required_problem_says_the_same_thing_at_both_statuses() {
        let read = no_session(409, "media", "nightly-1");
        let probe = no_session(404, "media", "nightly-1");
        assert_eq!(read.status(), 409);
        assert_eq!(probe.status(), 404);
        for error in [&read, &probe] {
            assert_eq!(error.0.r#type, "urn:kopiur:problem:session-required");
            assert!(
                error.0.what.contains("media/nightly-1"),
                "{:?}",
                error.0.what
            );
            assert!(
                error.0.fix.contains("start a browse session"),
                "{:?}",
                error.0.fix
            );
        }
    }

    #[test]
    fn a_failed_session_pods_log_tail_is_redacted_before_it_reaches_a_browser() {
        let error = session_error(OpsError::SessionPodFailed {
            job: "kopiur-browse-nas-0badc0de".into(),
            namespace: "media".into(),
            detail: "connecting: AWS_SECRET_ACCESS_KEY=hunter2hunter2 refused".into(),
        });
        assert!(
            !error.0.detail.contains("hunter2hunter2"),
            "a credential must never reach the browser: {}",
            error.0.detail
        );
        assert!(
            error.0.detail.contains("AWS_SECRET_ACCESS_KEY"),
            "{}",
            error.0.detail
        );

        // Every other ops failure passes through the shared mapping untouched.
        let plain = session_error(OpsError::PathNotFound {
            path: "sub/nope".into(),
        });
        assert_eq!(plain.status(), 404);
    }

    #[test]
    fn the_ready_timeout_names_the_budget_and_the_knob() {
        let error = session_ready_timeout(Duration::from_secs(300));
        assert_eq!(error.status(), 504);
        assert!(error.0.what.contains("300s"), "{:?}", error.0.what);
        assert!(
            error.0.fix.contains("KOPIUR_UI_SESSION_READY_TIMEOUT"),
            "{:?}",
            error.0.fix
        );
    }

    // --- query parsing ------------------------------------------------------

    /// Parse a query string exactly the way the router does — through axum's
    /// `Query`, so the test exercises the real extractor and not a second
    /// deserializer that happens to agree with it.
    fn tree_query(qs: &str) -> Result<TreeQuery, axum::extract::rejection::QueryRejection> {
        let uri: axum::http::Uri = format!("http://x/tree?{qs}").parse().expect("test uri");
        Query::<TreeQuery>::try_from_uri(&uri).map(|Query(q)| q)
    }

    #[test]
    fn the_tree_query_defaults_to_the_root_and_page_one() {
        let q = tree_query("").expect("an empty query is the root");
        assert_eq!((q.path.as_str(), q.offset, q.limit), ("", 0, None));

        let q = tree_query("path=sub%2Fdir&offset=10&limit=25").expect("full query");
        assert_eq!(
            (q.path.as_str(), q.offset, q.limit),
            ("sub/dir", 10, Some(25))
        );

        // Unknown parameters are refused rather than ignored, so a `?limits=`
        // typo is not silently answered with the default page size.
        assert!(tree_query("limits=25").is_err());
    }

    #[test]
    fn an_oversized_limit_is_clamped_rather_than_refused() {
        let clamp =
            |requested: Option<usize>| requested.unwrap_or(DEFAULT_TREE_LIMIT).min(MAX_TREE_LIMIT);
        assert_eq!(clamp(None), DEFAULT_TREE_LIMIT);
        assert_eq!(clamp(Some(10)), 10);
        assert_eq!(clamp(Some(usize::MAX)), MAX_TREE_LIMIT);
        const { assert!(DEFAULT_TREE_LIMIT <= MAX_TREE_LIMIT) };
    }
}

/// Router wiring, exercised through the real `axum` stack.
///
/// Everything asserted here happens **before** any apiserver call, which is what
/// makes it testable with no cluster — and is also the point: the CSRF gate, the
/// navigation check, and path validation are the three refusals that must never
/// depend on being able to reach the cluster first.
#[cfg(test)]
mod router_tests {
    use super::*;
    use std::sync::Arc;

    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    use crate::auth::csrf::{REQUEST_HEADER, REQUEST_HEADER_VALUE, SEC_FETCH_SITE};
    use crate::auth::{AuthState, identity_middleware};
    use crate::config::*;
    use kopiur_ui_model::problem::Problem;

    const SESSION: &str = "/api/v1/snapshots/media/nightly-1/session";
    const TREE: &str = "/api/v1/snapshots/media/nightly-1/tree";
    const FILE: &str = "/api/v1/snapshots/media/nightly-1/file";

    /// The browse router with the identity middleware in front, running as a
    /// fixed anonymous user whose kube client points at a closed port — so any
    /// handler that DID reach the cluster fails loudly rather than passing.
    fn app() -> Router {
        let state = AppState {
            cfg: Arc::new(config()),
            metrics: Arc::new(crate::metrics::UiMetrics::new(Arc::new(
                kopiur_telemetry::MetricsProvider::new("kopiur-ui-test"),
            ))),
            readiness: Arc::new(crate::ops_listener::Readiness::new(
                crate::static_files::is_placeholder(),
            )),
            auth: Arc::new(AuthState::default()),
            source: Arc::new(crate::cache::Source::Impersonated),
            sessions: Arc::new(session_pool::SessionPool::default()),
        };
        router()
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                identity_middleware,
            ))
            .with_state(state)
    }

    fn config() -> UiConfig {
        UiConfig {
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
            operator_namespace: Some("kopiur-system".to_string()),
            mover_image: None,
            cache_enabled: false,
            session: SessionLimits {
                ttl: Duration::from_secs(900),
                ready_timeout: Duration::from_secs(300),
                max_starts: DEFAULT_MAX_SESSION_STARTS,
                max_exec_per_identity: DEFAULT_MAX_EXEC_PER_IDENTITY,
                max_exec_global: DEFAULT_MAX_EXEC_GLOBAL,
            },
            download_max_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
            manifest_max_bytes: DEFAULT_MAX_MANIFEST_BYTES,
            snapshot_list_cap: DEFAULT_SNAPSHOT_LIST_CAP,
            client_cache: CacheLimits {
                size: DEFAULT_CLIENT_CACHE_SIZE,
                ttl: Duration::from_secs(600),
            },
            sar_ttl: Duration::from_secs(60),
            tls: None,
            cors_origins: Vec::new(),
        }
    }

    async fn send(request: Request<Body>) -> (StatusCode, Option<Problem>) {
        let response = app().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (status, serde_json::from_slice(&bytes).ok())
    }

    fn request(method: &str, uri: &str) -> axum::http::request::Builder {
        Request::builder().method(method).uri(uri)
    }

    /// The `type` URN of a problem body, or the empty string when the response
    /// was not one.
    fn kind(problem: Option<Problem>) -> String {
        problem.map(|p| p.r#type).unwrap_or_default()
    }

    #[tokio::test]
    async fn every_browse_route_is_mounted_with_exactly_its_documented_methods() {
        // A mounted path answers its own methods and 405s the rest; an unmounted
        // one 404s. That difference is what distinguishes "wired" from "typo'd".
        //
        // The marker header is sent because `route_layer` runs on a matched
        // PATH — a mutating method the router does not serve is refused by the
        // CSRF gate before the method router ever sees it, which is a correct
        // answer but not the one under test here.
        for (method, uri) in [
            ("PUT", SESSION),
            ("PATCH", SESSION),
            ("POST", TREE),
            ("DELETE", TREE),
            ("POST", FILE),
            ("PUT", "/api/v1/repositories/Repository/nas/session"),
            ("POST", "/api/v1/repositories/Repository/nas/session"),
        ] {
            let (status, _) = send(
                request(method, uri)
                    .header(REQUEST_HEADER, REQUEST_HEADER_VALUE)
                    .body(Body::empty())
                    .expect("req"),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {uri} must be a 405 on a mounted path"
            );
        }

        for uri in [
            "/api/v1/snapshots/media/nightly-1/nope",
            "/api/v1/snapshots/media/session",
            "/api/v1/repositories/Repository/media/nas/session",
        ] {
            let (status, _) = send(request("GET", uri).body(Body::empty()).expect("req")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri} is not a browse route");
        }
    }

    #[tokio::test]
    async fn starting_and_ending_a_session_is_behind_the_csrf_gate() {
        for method in ["POST", "DELETE"] {
            let (status, problem) =
                send(request(method, SESSION).body(Body::empty()).expect("req")).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {SESSION}");
            assert_eq!(
                kind(problem),
                "urn:kopiur:problem:csrf",
                "{method} must be refused without the SPA's marker header"
            );
        }

        let (status, problem) = send(
            request(
                "DELETE",
                "/api/v1/repositories/Repository/nas/session?namespace=media",
            )
            .body(Body::empty())
            .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(kind(problem), "urn:kopiur:problem:csrf");
    }

    #[tokio::test]
    async fn reading_is_not_behind_the_csrf_gate() {
        // The reads must stay reachable without the marker header: `GET …/file`
        // is a browser navigation that cannot carry one. Whatever these fail
        // with (they cannot reach a cluster here), it must not be the CSRF
        // refusal — that would make downloads impossible.
        for uri in [SESSION, TREE, FILE] {
            let (_status, problem) =
                send(request("GET", uri).body(Body::empty()).expect("req")).await;
            assert_ne!(kind(problem), "urn:kopiur:problem:csrf", "GET {uri}");
        }
    }

    #[tokio::test]
    async fn a_download_from_another_site_is_refused_before_the_cluster_is_touched() {
        let (status, problem) = send(
            request("GET", &format!("{FILE}?path=a.txt"))
                .header(SEC_FETCH_SITE, "cross-site")
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let problem = problem.expect("problem+json");
        assert_eq!(problem.r#type, "urn:kopiur:problem:csrf");
        assert!(problem.detail.contains("cross-site"), "{}", problem.detail);

        // `same-origin` (the SPA's own link) and `none` (a typed URL) get past
        // the gate — they then fail on the unreachable cluster, not here.
        for site in ["same-origin", "none"] {
            let (_status, problem) = send(
                request("GET", &format!("{FILE}?path=a.txt"))
                    .header(SEC_FETCH_SITE, site)
                    .body(Body::empty())
                    .expect("req"),
            )
            .await;
            assert_ne!(
                kind(problem),
                "urn:kopiur:problem:csrf",
                "Sec-Fetch-Site: {site}"
            );
        }
    }

    #[tokio::test]
    async fn a_path_that_escapes_the_snapshot_is_refused_before_the_cluster_is_touched() {
        for (uri, path) in [(TREE, "/etc/passwd"), (TREE, "a/../../b"), (FILE, "..")] {
            let (status, problem) = send(
                request("GET", &format!("{uri}?path={}", urlencode(path)))
                    .body(Body::empty())
                    .expect("req"),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}?path={path}");
            assert_eq!(kind(problem), "urn:kopiur:problem:invalid");
        }
    }

    #[tokio::test]
    async fn a_mistyped_query_parameter_is_refused_rather_than_ignored() {
        // `?limits=1` silently answered with the default page size is the kind
        // of bug a user never reports and never works around.
        let (status, _) = send(
            request("GET", &format!("{TREE}?limits=1"))
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_session_body_that_is_not_the_documented_shape_is_a_400() {
        let (status, problem) = send(
            request("POST", SESSION)
                .header(REQUEST_HEADER, REQUEST_HEADER_VALUE)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"ttl_seconds": 300}"#))
                .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(kind(problem), "urn:kopiur:problem:invalid");
    }

    #[tokio::test]
    async fn ending_a_repository_session_needs_enough_to_find_it() {
        // Past the CSRF gate, but naming a namespaced Repository with no
        // namespace: refused before any lookup, because there is nothing to
        // look up.
        let (status, problem) = send(
            request("DELETE", "/api/v1/repositories/Repository/nas/session")
                .header(REQUEST_HEADER, REQUEST_HEADER_VALUE)
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let problem = problem.expect("problem+json");
        assert!(problem.fix.contains("namespace="), "{}", problem.fix);

        // A ClusterRepository has no namespace of its own, so the session's
        // must be named.
        let (status, problem) = send(
            request(
                "DELETE",
                "/api/v1/repositories/ClusterRepository/nas/session",
            )
            .header(REQUEST_HEADER, REQUEST_HEADER_VALUE)
            .body(Body::empty())
            .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let problem = problem.expect("problem+json");
        assert!(problem.fix.contains("sessionNamespace="), "{}", problem.fix);

        // An unknown kind never becomes a lookup either.
        let (status, _) = send(
            request(
                "DELETE",
                "/api/v1/repositories/Secret/nas/session?namespace=media",
            )
            .header(REQUEST_HEADER, REQUEST_HEADER_VALUE)
            .body(Body::empty())
            .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// Minimal percent-encoding for the test URIs above.
    fn urlencode(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect()
    }
}
