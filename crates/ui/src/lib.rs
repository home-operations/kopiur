#![warn(missing_docs)]
//! `kopiur-ui` — the web UI backend for the kopiur operator.
//!
//! It serves two things on one port: the embedded single-page app
//! ([`static_files`]) and a JSON API over the nine Kopiur CRDs ([`api`]). It is
//! *not* a controller — it never reconciles, never holds a lease, and never runs
//! kopia itself. Long operations stay where they were: browse sessions are mover
//! pods the operator already knows how to run, and every action the UI takes is
//! the same CR write `kubectl kopiur` would have made, built from the same
//! `kopiur_ops` request types.
//!
//! # The one thing to understand before changing anything here
//!
//! **The UI authorizes nothing.** It resolves an identity from trusted proxy
//! headers and then makes every apiserver call *impersonating* that identity, so
//! RBAC is enforced by the apiserver against the human, not against the UI's
//! ServiceAccount. That is why the UI's own permissions are `impersonate` (plus,
//! optionally, read access for the cache) and nothing else — no Secrets, ever —
//! and why a bug in a handler can leak nothing the caller could not already read.
//!
//! Two consequences shape the whole crate. Configuration fails closed
//! ([`config::UiArgs::resolve`]): a setup where identity is ambiguous refuses to
//! start rather than defaulting to the UI's own powerful ServiceAccount. And the
//! optional read cache — shared reflector stores filled under the UI's own
//! ServiceAccount, which the apiserver therefore did *not* filter per caller — is
//! gated by `SubjectAccessReview` on every read ([`cache::authz`]), because
//! caching is the one place the impersonation guarantee could otherwise be lost.

pub mod actions;
pub mod api;
pub mod auth;
pub mod browse;
pub mod cache;
pub mod config;
pub mod metrics;
pub mod ops_listener;
pub mod startup;
pub mod static_files;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{HeaderName, HeaderValue, Method, Response, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::routing::any;

use tokio::sync::Semaphore;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::compression::{CompressionLayer, DefaultPredicate, Predicate};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::api::problem::{ApiError, problem, request_path};
use crate::auth::{ServedAs, identity_middleware};
use crate::config::UiConfig;
use crate::metrics::UiMetrics;
use crate::ops_listener::Readiness;

/// Everything a request handler needs, cloned into every route.
///
/// All `Arc`, so cloning is a refcount bump: axum clones the state per request.
#[derive(Clone)]
pub struct AppState {
    /// The resolved configuration. Nothing downstream re-reads the environment.
    pub cfg: Arc<UiConfig>,
    /// Metrics recorders, sharing the ops listener's provider.
    pub metrics: Arc<UiMetrics>,
    /// The flags `/readyz` reports on.
    pub readiness: Arc<Readiness>,
    /// Identity resolution and the impersonating client cache.
    pub auth: Arc<auth::AuthState>,
    /// Where **reads** come from: the reflector cache or a live impersonated
    /// call.
    ///
    /// Reads only. A mutation's input is not a view: the `SnapshotPolicy` that
    /// `snapshot-now` reads decides how many `Snapshot` CRs get created and
    /// against which repositories, so serving it from a store that is one watch
    /// lag behind would not misrender a page, it would create the wrong backups.
    /// `actions/` therefore contains zero references to this field, and a test
    /// there greps its own source to keep it that way.
    pub source: Arc<cache::Source>,
    /// Browse-session start and exec bounds.
    pub sessions: Arc<browse::session_pool::SessionPool>,
}

/// The app router: the SPA plus `/api/v1`, with every shared layer applied.
///
/// # How the routers fit together
///
/// Three routers, mounted two different ways, because two of them register
/// *relative* paths and one registers *absolute* ones:
///
/// * [`api::router`] and [`actions::router`] use relative paths (`/graph`,
///   `/actions/suspend`), so they are merged together and `nest`ed under
///   `/api/v1`.
/// * [`browse::router`] registers absolute paths already
///   (`/api/v1/snapshots/{namespace}/{name}/tree`), because a browse route's
///   shape is not a suffix of one prefix. It is therefore `merge`d at the root —
///   nesting it would produce `/api/v1/api/v1/…`.
///
/// **A merged router inherits nothing from a nest point.** That is the whole
/// reason [`api_surface`] builds the merged whole first and layers *that*: a
/// browse route outside `identity_middleware` would not 401, it would reach a
/// handler and 500 on [`auth::CurrentIdentity`], and a browse route without the
/// security headers would be a silent regression nobody notices.
///
/// # Layer order, outermost first
///
/// | layer | scope | why it sits where it does |
/// |---|---|---|
/// | [`TraceLayer`] | everything | one span per request, recording method and the *matched* path only |
/// | `Referrer-Policy` | everything | Kopiur URLs carry namespaces and object names |
/// | `X-Frame-Options`, `X-Content-Type-Options`, CSP | everything | the app **shell** is what an attacker frames, so these belong on the HTML document at least as much as on the JSON |
/// | [`CompressionLayer`] | everything but a download | gzip for the SPA bundle and JSON; see [`compressible`] |
/// | `Cache-Control: no-store` | `/api` | the one header that must NOT reach the SPA: it would defeat the year-long `immutable` on the content-hashed assets |
/// | [`CorsLayer`] | `/api` | only when `KOPIUR_UI_CORS_ORIGINS` is set, which is a dev-server affordance |
/// | [`RequestBodyLimitLayer`] | `/api` | 64 KiB; the biggest legitimate body is a `Restore` request |
/// | [`record_request`] | `/api`, catch-alls included | outside identity, so it sees the 401s and 403s identity produces — and outside the timeout, so it counts the 504s |
/// | [`with_timeout`] | `/api` **except** `…/file` | a fixed deadline is the wrong bound for a multi-gigabyte download |
/// | [`GlobalConcurrencyLimitLayer`] | `/api` routes | **inside** the timeout, so time spent queueing for a permit counts against the budget; one semaphore shared by both halves |
/// | [`identity_middleware`] | `/api` routes | innermost: everything above runs whether or not a caller was resolved |
///
/// The timeout/concurrency ordering is the load-bearing one. With the limit
/// outside, a request that arrives when all 256 permits are taken waits in
/// `poll_ready` under no deadline at all and only *then* starts its 30 seconds —
/// so a saturated server queues indefinitely instead of shedding. Inside, the
/// budget covers the wait, and an over-subscribed UI answers 504 rather than
/// growing an unbounded backlog.
///
/// Anything that matched no route falls through to [`static_files::spa_fallback`]
/// so a deep link is handled by the client-side router — except under `/api`,
/// which is claimed explicitly and answers [`api_not_found`]. An API call
/// answered with 200 and an HTML page would leave the SPA parsing
/// `<!doctype html>` as JSON.
pub fn app(state: AppState) -> Router {
    app_with(state, config::API_REQUEST_TIMEOUT)
}

/// [`app`] with the request timeout as a parameter.
///
/// The seam exists for one reason: it is the only way to assert that the *real*
/// composition carries the timeout on the timed half and not on `…/file`. A test
/// that mounts its own router around [`with_timeout`] proves the middleware
/// works and proves nothing about where it was applied — deleting the
/// `route_layer` here would leave such a test green.
fn app_with(state: AppState, budget: Duration) -> Router {
    let cors = cors_layer(&state.cfg.cors_origins);

    Router::new()
        .merge(api_surface(state.clone(), cors, budget))
        .fallback(static_files::spa_fallback)
        .layer(CompressionLayer::new().compress_when(compressible()))
        // The three headers that belong on the app SHELL as much as on the API.
        // `X-Frame-Options`/`frame-ancestors` are about what may embed the
        // document, and the document an attacker wants to frame is the HTML one;
        // `nosniff` is about not letting a browser reinterpret a body, which is
        // as true of an asset as of a JSON payload. `Cache-Control` is the one
        // that stays `/api`-scoped, in `api_surface`.
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            static_header(config::X_CONTENT_TYPE_OPTIONS),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            static_header(config::X_FRAME_OPTIONS),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            static_header(config::CONTENT_SECURITY_POLICY),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            static_header(config::REFERRER_POLICY),
        ))
        .layer(TraceLayer::new_for_http().make_span_with(request_span))
        .with_state(state)
}

/// Everything under `/api`, with the layers that must not reach the SPA.
///
/// Split out of [`app`] so the scoping is structural rather than a comment: a
/// layer added here cannot accidentally apply to the static bundle, and a layer
/// added in [`app`] cannot accidentally miss a browse route.
///
/// `route_layer` rather than `layer` for everything that must not run on an
/// unmatched path: `layer` also wraps the fallback, so an unknown
/// `/api/v1/nope` would be answered by the identity middleware (401) instead of
/// by [`api_not_found`] (404) — an honest 404 is what tells an operator their
/// bundle and their backend disagree.
///
/// The identity, concurrency and timeout layers are applied **per half** rather
/// than to the merged whole. They have to be: `…/file` needs the concurrency
/// limit and the identity middleware but must escape the timeout, and the
/// timeout has to sit *outside* the limit, so the two halves cannot share one
/// stack. The semaphore is shared explicitly so "one limit" survives the split.
fn api_surface(state: AppState, cors: Option<CorsLayer>, budget: Duration) -> Router<AppState> {
    // ONE semaphore across both halves. Two `ConcurrencyLimitLayer`s would mean
    // 2×256 in flight, and the download route — the most expensive thing the UI
    // serves — would be the half with its own private budget.
    let permits = Arc::new(Semaphore::new(config::MAX_CONCURRENT_API_REQUESTS));
    let concurrency = GlobalConcurrencyLimitLayer::with_semaphore(Arc::clone(&permits));

    let timed = Router::new()
        .nest("/api/v1", api::router().merge(actions::router()))
        .merge(browse::router())
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity_middleware,
        ))
        .route_layer(concurrency.clone())
        .route_layer(axum::middleware::from_fn(move |req, next| {
            with_timeout(budget, req, next)
        }));

    // Same stack minus the timeout. `…/file` streams a restore that legitimately
    // outlives any fixed deadline and bounds itself on progress instead.
    let untimed = browse::router_untimed()
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity_middleware,
        ))
        .route_layer(concurrency);

    let bounded = timed
        .merge(untimed)
        // Registered AFTER the per-half layers and BEFORE the metrics layer, so
        // an unknown `/api` path is a 404 rather than a 401 and is still counted.
        .route("/api", any(api_not_found))
        .route("/api/{*rest}", any(api_not_found))
        .route_layer(axum::middleware::from_fn_with_state(state, record_request))
        .layer(RequestBodyLimitLayer::new(config::MAX_REQUEST_BODY_BYTES))
        // axum's own extractors consult `DefaultBodyLimit`, which defaults to
        // 2 MiB and would otherwise reject before — and differently from — the
        // layer above. One number, one refusal.
        .layer(DefaultBodyLimit::max(config::MAX_REQUEST_BODY_BYTES));

    // `Option<CorsLayer>` is not itself a `Layer`, and `tower::util::option_layer`
    // would pull in a feature for one call. A match keeps the "absent means
    // absent" reading obvious.
    let bounded = match cors {
        Some(cors) => bounded.layer(cors),
        None => bounded,
    };

    bounded.layer(SetResponseHeaderLayer::overriding(
        header::CACHE_CONTROL,
        static_header(config::API_CACHE_CONTROL),
    ))
}

/// A `HeaderValue` for one of the fixed policy strings in [`config`].
///
/// Every caller passes a crate constant of visible ASCII, so the parse cannot
/// fail; an empty value rather than a panic keeps a typo in a constant from
/// taking the process down at the first request.
fn static_header(value: &'static str) -> Option<HeaderValue> {
    match HeaderValue::from_str(value) {
        Ok(header) => Some(header),
        Err(e) => {
            tracing::error!(
                value,
                error = %e,
                "a kopiur-ui security header constant is not a valid header value; the header \
                 will be omitted. This is a bug in kopiur-ui — report it."
            );
            None
        }
    }
}

/// The dev-only CORS layer, or `None` when no origin is configured.
///
/// Production is same-origin — the SPA is served by this very process — so an
/// unset `KOPIUR_UI_CORS_ORIGINS` is the normal case and the one that keeps the
/// `Sec-Fetch-Site`/`Origin` half of the CSRF posture meaningful. When origins
/// *are* named they are named exactly: never `Any`, because credentialed
/// requests plus a wildcard origin is the combination browsers refuse anyway and
/// that a reviewer must never have to think twice about.
fn cors_layer(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }
    let allowed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| match HeaderValue::from_str(origin) {
            Ok(value) => Some(value),
            Err(e) => {
                tracing::error!(
                    origin,
                    error = %e,
                    "ignoring an entry of KOPIUR_UI_CORS_ORIGINS that is not a valid origin; \
                     write it as a scheme and authority, e.g. http://localhost:5173"
                );
                None
            }
        })
        .collect();

    if allowed.is_empty() {
        tracing::error!(
            "KOPIUR_UI_CORS_ORIGINS named only unusable values, so no cross-origin request \
             will be allowed; write each entry as a scheme and authority, e.g. \
             http://localhost:5173"
        );
        return None;
    }

    tracing::warn!(
        origins = ?origins,
        "cross-origin requests to /api are ENABLED. This is a development affordance for a \
         Vite dev server on another port; unset KOPIUR_UI_CORS_ORIGINS in production, where \
         the SPA is same-origin."
    );
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(allowed))
            .allow_credentials(true)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::DELETE,
                Method::HEAD,
                Method::OPTIONS,
            ])
            .allow_headers([
                header::CONTENT_TYPE,
                HeaderName::from_static(auth::csrf::REQUEST_HEADER),
            ]),
    )
}

/// Compress everything tower-http would, except a file being handed to a user.
///
/// A download commits a `Content-Length` before it streams — that is what makes
/// a truncated transfer visible to the browser instead of silently saved — and
/// compressing the response replaces it with a chunked body, destroying exactly
/// that guarantee. `Content-Disposition: attachment` is the marker, because a
/// tower-http predicate sees the *response*, never the request path: it is also
/// the more durable rule, since any future attachment response inherits it.
fn compressible() -> impl Predicate {
    DefaultPredicate::new().and(
        |_status: StatusCode,
         _version: axum::http::Version,
         headers: &axum::http::HeaderMap,
         _extensions: &axum::http::Extensions| {
            !headers
                .get(header::CONTENT_DISPOSITION)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.trim_start().starts_with("attachment"))
        },
    )
}

/// One span per request, carrying the method and the **matched route template**.
///
/// Never the concrete path: it carries namespaces and object names, which are
/// both fleet topology and — through a browse `?path=` — a user's filenames.
fn request_span(request: &Request<Body>) -> tracing::Span {
    tracing::info_span!(
        "http",
        method = %request.method(),
        route = request
            .extensions()
            .get::<MatchedPath>()
            .map_or(config::UNMATCHED_ROUTE, MatchedPath::as_str),
    )
}

/// Bound how long an `/api` request may run, and refuse it with a problem.
///
/// `tower_http::timeout` would answer a bare 408 with an empty body, which the
/// SPA cannot render and an operator cannot act on. Every other refusal in this
/// crate is an `application/problem+json` with what/why/fix, and a timeout is
/// the one a person is most likely to actually see.
///
/// 504 rather than 408: the work happened here, not in a client that was slow to
/// send its request, and 504 is already this crate's `timeout` problem class.
/// `…/file` never reaches this middleware — see [`browse::router_untimed`].
///
/// The budget is a parameter rather than read from [`config`] so the real
/// composition can be exercised with a short one; [`app`] always passes
/// [`config::API_REQUEST_TIMEOUT`].
async fn with_timeout(budget: Duration, req: Request, next: Next) -> Response<Body> {
    // `request_path`, not `req.uri()`. Correct either way today — this is applied
    // by `route_layer`, outside the `StripPrefix` that `nest` puts on the
    // endpoint — but it is the same class of bug the rest of the crate fixed, and
    // it would regress silently if the layer ever moved inside the nest.
    let path = request_path(req.extensions(), req.uri());
    match tokio::time::timeout(budget, next.run(req)).await {
        Ok(response) => response,
        Err(_) => timed_out(&path, budget).into_response(),
    }
}

/// The problem an over-running request is answered with.
fn timed_out(path: &str, budget: Duration) -> ApiError {
    problem(
        504,
        "timeout",
        format!(
            "kopiur-ui gave up on {path} after {} seconds.",
            budget.as_secs()
        ),
        "A bounded wait expired. The work itself may still be running in the cluster — the \
         request was abandoned, not undone.",
        "reload the page in a moment; if it keeps happening, check the apiserver's health and \
         whether this cluster holds far more Kopiur objects than the UI's list caps expect",
    )
    .with_instance(path.to_string())
}

/// Count one answered `/api` request on `kopiur_ui_requests_total`.
///
/// Mounted OUTSIDE [`identity_middleware`], which is what lets one recording
/// site cover both the responses handlers produce and the 401/403 the identity
/// layer produces on its own. That places it on the wrong side of the request,
/// though — the request, and the [`auth::identity::Identity`] in it, is moved
/// into the inner service — so the identity layer publishes what it resolved on
/// the *response* as [`ServedAs`] and this reads it back.
///
/// `route` is the matched route TEMPLATE. Labelling the raw path would make the
/// series cardinality the size of the fleet, and an unmatched path is
/// caller-controlled, so anyone could mint series at will.
async fn record_request(State(app): State<AppState>, req: Request, next: Next) -> Response<Body> {
    let route = req.extensions().get::<MatchedPath>().map_or_else(
        || config::UNMATCHED_ROUTE.to_string(),
        |m| m.as_str().to_string(),
    );

    let response = next.run(req).await;

    let source = response
        .extensions()
        .get::<ServedAs>()
        .map_or_else(|| app.auth.mode_source(), |served| served.0.clone());
    app.metrics
        .inc_request(&route, response.status().as_u16(), &source);
    app.metrics.set_identity_cache_size(app.auth.clients.len());
    response
}

/// 404 for an `/api` path that no handler claims.
async fn api_not_found(uri: Uri) -> Response<Body> {
    problem(
        StatusCode::NOT_FOUND.as_u16(),
        "not-found",
        format!(
            "The UI asked for {}, which this backend does not serve.",
            uri.path()
        ),
        "The path is not part of the kopiur-ui API surface — usually a stale SPA bundle \
         calling an endpoint a newer or older backend has, or a typo in a hand-made request.",
        "reload the page to pick up the bundle this backend ships, and make sure the \
         kopiur-ui image and the operator come from the same release",
    )
    .with_instance(uri.path().to_string())
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body as ReqBody;
    use axum::http::Request as HttpRequest;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    fn test_state() -> AppState {
        // A closed port: any handler that reached the cluster fails loudly
        // rather than passing for the wrong reason.
        test_state_at("http://127.0.0.1:1/")
    }

    fn test_state_at(cluster_url: &str) -> AppState {
        let provider = Arc::new(kopiur_telemetry::MetricsProvider::new("kopiur-ui-test"));
        let cfg = test_config();
        AppState {
            metrics: Arc::new(UiMetrics::new(provider)),
            readiness: Arc::new(Readiness::new(static_files::is_placeholder())),
            auth: Arc::new(auth::AuthState::new(
                cfg.auth.clone(),
                kube::Config::new(cluster_url.parse().expect("a literal URL parses as a Uri")),
                cfg.client_cache.clone(),
            )),
            source: Arc::new(cache::Source::Impersonated),
            sessions: Arc::new(browse::session_pool::SessionPool::default()),
            cfg: Arc::new(cfg),
        }
    }

    fn test_config() -> UiConfig {
        use crate::config::*;
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
            operator_namespace: None,
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
            download_chunk_timeout: Duration::from_secs(60),
            manifest_max_bytes: DEFAULT_MAX_MANIFEST_BYTES,
            snapshot_list_cap: DEFAULT_SNAPSHOT_LIST_CAP,
            client_cache: CacheLimits {
                size: DEFAULT_CLIENT_CACHE_SIZE,
                ttl: Duration::from_secs(600),
            },
            sar_ttl: Duration::from_secs(60),
            sar_cache_size: crate::config::DEFAULT_SAR_CACHE_SIZE,
            tls: None,
            cors_origins: Vec::new(),
        }
    }

    #[tokio::test]
    async fn an_unmounted_api_path_is_a_problem_json_404_not_the_spa() {
        let response = app(test_state())
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/nothing-here")
                    .body(ReqBody::empty())
                    .expect("test request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json"),
            "an API call must never be answered with the SPA's HTML"
        );

        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let problem: kopiur_ui_model::problem::Problem =
            serde_json::from_slice(&body).expect("problem+json");
        assert_eq!(problem.status, 404);
        assert_eq!(
            problem.r#type, "urn:kopiur:problem:not-found",
            "one problem vocabulary, not two"
        );
        assert_eq!(problem.instance.as_deref(), Some("/api/v1/nothing-here"));
        assert!(
            !problem.fix.is_empty(),
            "every problem carries a remediation"
        );
    }

    #[tokio::test]
    async fn a_browser_route_falls_through_to_the_spa() {
        let response = app(test_state())
            .oneshot(
                HttpRequest::builder()
                    .uri("/snapshots/prod/nightly-1")
                    .body(ReqBody::empty())
                    .expect("test request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html"),
            "deep links are handled by the client-side router, not by a 404"
        );
    }

    /// `Cache-Control` is the ONE header scoped to `/api`. `no-store` on the SPA
    /// would re-download the content-hashed bundle on every navigation — the
    /// exact opposite of what the year-long `immutable` on `assets/` is for.
    ///
    /// The framing and sniffing headers go the other way: the app **shell** is
    /// the document an attacker wants to iframe, so scoping those to the JSON
    /// API would have protected the half nobody frames and left the half they do.
    #[tokio::test]
    async fn the_spa_keeps_its_caching_but_still_carries_the_framing_headers() {
        let response = app(test_state())
            .oneshot(
                HttpRequest::builder()
                    .uri("/snapshots/prod/nightly-1")
                    .body(ReqBody::empty())
                    .expect("test request"),
            )
            .await
            .expect("response");

        let header = |name: header::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };

        assert_eq!(
            header(header::CACHE_CONTROL),
            Some("no-cache".to_string()),
            "the SPA keeps static_files' own caching policy"
        );
        assert_eq!(
            header(header::X_FRAME_OPTIONS),
            Some(config::X_FRAME_OPTIONS.to_string()),
        );
        assert_eq!(
            header(header::X_CONTENT_TYPE_OPTIONS),
            Some(config::X_CONTENT_TYPE_OPTIONS.to_string()),
        );
        assert_eq!(
            header(header::CONTENT_SECURITY_POLICY),
            Some(config::CONTENT_SECURITY_POLICY.to_string()),
            "the shell is what frame-ancestors 'none' has to protect",
        );
        assert_eq!(
            header(header::REFERRER_POLICY),
            Some(config::REFERRER_POLICY.to_string()),
            "Kopiur URLs carry namespaces and object names",
        );
    }

    #[test]
    fn a_download_is_never_compressed() {
        use axum::http::HeaderMap;

        let predicate = compressible();
        let mut attachment = HeaderMap::new();
        attachment.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"holiday.tar\""),
        );
        attachment.insert(header::CONTENT_LENGTH, HeaderValue::from_static("40000000"));
        attachment.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );

        // A real body: tower-http's `SizeAbove` reads the body's size hint before
        // it reads `Content-Length`, so an empty body would be skipped for a
        // reason that has nothing to do with the rule under test.
        let response = |headers: HeaderMap| {
            let mut r = Response::new(Body::from(vec![b'x'; 4096]));
            *r.headers_mut() = headers;
            r
        };

        assert!(
            !predicate.should_compress(&response(attachment)),
            "compressing a download drops the Content-Length that makes a truncated \
             transfer visible to the browser",
        );

        let mut json = HeaderMap::new();
        json.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        json.insert(header::CONTENT_LENGTH, HeaderValue::from_static("40000"));
        assert!(
            predicate.should_compress(&response(json)),
            "everything else still compresses — the SPA bundle is the reason the layer exists",
        );
    }

    #[test]
    fn cors_is_off_unless_an_origin_is_named_and_never_accepts_a_bad_one() {
        assert!(
            cors_layer(&[]).is_none(),
            "production is same-origin; an always-on CORS layer would undercut the \
             Sec-Fetch-Site half of the CSRF posture",
        );
        assert!(cors_layer(&["http://localhost:5173".to_string()]).is_some());
        assert!(
            cors_layer(&["not a header value\n".to_string()]).is_none(),
            "an unusable origin must leave CORS off rather than open",
        );
    }

    #[test]
    fn every_security_header_constant_is_a_usable_header_value() {
        for value in [
            config::API_CACHE_CONTROL,
            config::CONTENT_SECURITY_POLICY,
            config::X_FRAME_OPTIONS,
            config::X_CONTENT_TYPE_OPTIONS,
            config::REFERRER_POLICY,
        ] {
            assert!(
                static_header(value).is_some(),
                "{value:?} must survive HeaderValue::from_str",
            );
        }
        assert!(
            config::CONTENT_SECURITY_POLICY.contains("frame-ancestors 'none'"),
            "the CSP must forbid embedding: {}",
            config::CONTENT_SECURITY_POLICY,
        );
    }

    /// The timed half really does time out, and what comes back is a problem
    /// document rather than the bare 408 `tower_http::timeout` would produce: a
    /// timeout is the refusal a person is most likely to actually see, and it
    /// has to say that the work may still be running.
    ///
    /// A tiny budget over a handler that sleeps, which is exactly what the
    /// router does with a 30-second one.
    #[tokio::test]
    async fn a_handler_that_outruns_its_budget_is_a_timeout_problem() {
        use axum::routing::get;

        let router = Router::new()
            .route(
                "/api/v1/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "never reached"
                }),
            )
            .layer(axum::middleware::from_fn(|req, next| {
                with_timeout(Duration::from_millis(20), req, next)
            }));

        let response = router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/slow")
                    .body(ReqBody::empty())
                    .expect("test request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json"),
            "the SPA switches on the content type to decide whether a body is an error",
        );

        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let problem: kopiur_ui_model::problem::Problem =
            serde_json::from_slice(&body).expect("problem+json");
        assert_eq!(problem.r#type, "urn:kopiur:problem:timeout");
        assert_eq!(problem.instance.as_deref(), Some("/api/v1/slow"));
    }

    /// `…/file` is deliberately NOT behind [`api_timeout`], because a legitimate
    /// multi-gigabyte restore outlives any budget that is still useful for a JSON
    /// call. Asserted on the routers themselves: the download route is in the
    /// untimed half and nothing else is.
    #[test]
    fn only_the_download_route_is_exempt_from_the_timeout() {
        let untimed = format!("{:?}", browse::router_untimed());
        assert!(untimed.contains("/file"), "{untimed}");
        for timed_only in ["/tree", "/session"] {
            assert!(
                !untimed.contains(timed_only),
                "{timed_only} must stay behind the request timeout: {untimed}",
            );
        }
        let timed = format!("{:?}", browse::router());
        assert!(
            !timed.contains("/file"),
            "the download must not be registered twice: {timed}",
        );
    }

    #[test]
    fn the_timeout_problem_names_the_budget_and_what_it_means() {
        let error = timed_out("/api/v1/graph", Duration::from_secs(30));
        assert_eq!(error.0.status, 504);
        assert_eq!(error.0.r#type, "urn:kopiur:problem:timeout");
        assert!(error.0.what.contains("30"), "{}", error.0.what);
        assert!(error.0.what.contains("/api/v1/graph"), "{}", error.0.what);
        assert!(
            error.0.why.contains("may still be running"),
            "a timeout must not imply the work was undone: {}",
            error.0.why
        );
        assert_eq!(error.0.instance.as_deref(), Some("/api/v1/graph"));
    }

    /// A TCP listener that accepts the handshake and never answers.
    ///
    /// The kernel completes the connection into the accept backlog, so a client
    /// connects successfully and then waits for a response that never comes —
    /// which is what a request stuck on a wedged apiserver looks like, and the
    /// only way to make a real handler outrun a deadline without a fake clock.
    fn black_hole() -> (tokio::net::TcpListener, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
        let addr = listener
            .local_addr()
            .expect("a bound listener has an address");
        listener
            .set_nonblocking(true)
            .expect("tokio requires a non-blocking listener");
        let listener = tokio::net::TcpListener::from_std(listener).expect("adopt the std listener");
        (listener, format!("http://{addr}/"))
    }

    /// The REAL composition carries the timeout on the timed half.
    ///
    /// `a_handler_that_outruns_its_budget_is_a_timeout_problem` proves the
    /// middleware works; it mounts its own router, so deleting the `route_layer`
    /// in `api_surface` would leave it green. This drives `app_with` — the same
    /// function `app` calls, with a short budget — against a handler that really
    /// does hang, so the assertion is about where the layer was applied.
    #[tokio::test]
    async fn the_timed_half_of_the_real_router_times_out() {
        let (_listener, url) = black_hole();
        let router = app_with(test_state_at(&url), Duration::from_millis(50));

        // Bounded from the outside as well. Without the layer under test the
        // request never completes at all, and a test that hangs forever is a
        // test that reports nothing; this turns that regression into a failure.
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            router.oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/snapshots/media/nightly-1/tree")
                    .body(ReqBody::empty())
                    .expect("test request"),
            ),
        )
        .await
        .expect("the router's own 50ms budget must answer long before this one")
        .expect("response");

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let problem: kopiur_ui_model::problem::Problem =
            serde_json::from_slice(&body).expect("problem+json");
        assert_eq!(problem.r#type, "urn:kopiur:problem:timeout");
        assert_eq!(
            problem.instance.as_deref(),
            Some("/api/v1/snapshots/media/nightly-1/tree"),
            "the instance must name the path the caller asked for",
        );
    }

    /// …and `…/file` really is exempt from it.
    ///
    /// The assertion is the negative one, so it is made by *outliving* the
    /// budget: with the same 50 ms timeout applied, a download against the same
    /// wedged cluster must still be in flight an order of magnitude later. A
    /// timeout that leaked onto this route would have answered 504 long before.
    #[tokio::test]
    async fn the_download_route_of_the_real_router_is_not_timed() {
        let (_listener, url) = black_hole();
        let router = app_with(test_state_at(&url), Duration::from_millis(50));

        let request = router.oneshot(
            HttpRequest::builder()
                .uri("/api/v1/snapshots/media/nightly-1/file?path=a.txt")
                .header("sec-fetch-site", "same-origin")
                .body(ReqBody::empty())
                .expect("test request"),
        );

        let outcome = tokio::time::timeout(Duration::from_millis(600), request).await;
        assert!(
            outcome.is_err(),
            "…/file must outlive the request budget — a legitimate multi-gigabyte restore \
             outruns any fixed deadline, so this route bounds itself on progress instead. \
             It answered: {:?}",
            outcome.map(|r| r.map(|response| response.status())),
        );
    }

    /// The compression exemption is keyed on `Content-Disposition: attachment`,
    /// and the download route is what has to produce one. Nothing coupled the
    /// two halves before: `a_download_is_never_compressed` hand-built the header,
    /// so a change to how downloads are framed could have silently started
    /// gzipping them — which drops the `Content-Length` the transfer commits and
    /// with it the browser's only way to notice a truncated file.
    #[test]
    fn the_real_download_header_is_what_the_predicate_refuses() {
        use axum::http::HeaderMap;

        // Names that exercise every branch of `content_disposition`, including
        // the non-ASCII path and the fallback.
        for name in ["holiday.tar", "ål og æbler.txt", "", "a\"quote\".bin"] {
            let disposition = browse::content_disposition(name);
            let rendered = disposition
                .to_str()
                .expect("Content-Disposition must stay ASCII on the wire");
            assert!(
                rendered.trim_start().starts_with("attachment"),
                "every download must be framed as an attachment, got {rendered:?} for {name:?}",
            );

            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_DISPOSITION, disposition);
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("40000000"));
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );

            let mut response = Response::new(Body::from(vec![b'x'; 4096]));
            *response.headers_mut() = headers;
            assert!(
                !compressible().should_compress(&response),
                "the real download header must be the one the predicate refuses ({name:?})",
            );
        }
    }

    /// The browse exec permit is taken AFTER the session-readiness wait.
    ///
    /// A deliberate ordering with no compile-time expression: what the permit
    /// bounds is concurrent kopia processes and apiserver websockets, and a
    /// request parked in `require_live_session` — up to
    /// `KOPIUR_UI_SESSION_READY_TIMEOUT`, and on one snapshot behind the pool's
    /// per-key single-flight lock — is neither. Taking it first let four idle
    /// browser tabs spend a whole identity's exec budget while nothing was
    /// running, and answered the fifth with a 429 that was not true.
    ///
    /// Asserted against the source because the behavioural version needs a live
    /// session pod. It lives here rather than in `browse` because it is a claim
    /// about a crate-level invariant, next to the composition that makes the
    /// rest of them.
    #[test]
    fn the_browse_exec_permit_is_taken_after_the_readiness_wait_and_before_the_exec() {
        let source = include_str!("browse/mod.rs");
        let body = source
            .split("#[cfg(test)]")
            .next()
            .expect("the non-test half of browse/mod.rs");

        for handler in ["async fn tree(", "async fn download("] {
            let start = body
                .find(handler)
                .unwrap_or_else(|| panic!("{handler} must exist"));
            let rest = &body[start..];
            let end = rest.find("\n}\n").unwrap_or(rest.len());
            let function = &rest[..end];

            let wait = function
                .find("require_live_session(")
                .unwrap_or_else(|| panic!("{handler} must wait for a live session"));
            let permit = function
                .find(".exec_permit(")
                .unwrap_or_else(|| panic!("{handler} must take an exec permit"));
            let first_exec = function
                .find("CappedAccess::new")
                .unwrap_or_else(|| panic!("{handler} must open a capped access"));

            assert!(
                wait < permit,
                "{handler}: the exec permit must be taken AFTER the readiness wait, so a \
                 request that is merely waiting does not hold one",
            );
            assert!(
                permit < first_exec,
                "{handler}: the permit must still be held BEFORE anything reaches the \
                 session pod — that guarantee is what the cap is for",
            );
        }
    }
}
