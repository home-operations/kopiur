//! What a browser actually sees: [`kopiur_ui::app`] with every layer on it.
//!
//! The unit tests in `crates/ui/src/**` each mount one router behind one
//! middleware, which is the right shape for testing a handler's decisions and
//! the wrong shape for testing the *wiring*. Almost every mistake this file
//! guards against is invisible there: a browse route merged outside the identity
//! layer (401 becomes a runtime 500), a security header applied to the nest point
//! instead of the merged whole (missing on exactly the routes that stream user
//! data), an unmatched `/api` path falling through to the SPA (the SPA parsing
//! `<!doctype html>` as JSON), a metrics label built from the raw path (unbounded
//! cardinality, from a path any caller controls).
//!
//! So everything here goes through the real `app()`, in header mode with a proxy
//! secret — the production posture — and every assertion reads the body and
//! checks the problem `type`, not just the status. A status code is not a
//! contract; the SPA switches on the `type` URN.
//!
//! # Why there is no fake apiserver
//!
//! There is nowhere to put one. `AppState` holds no `kube::Client`: handlers get
//! theirs from `AuthState`'s per-identity cache, built from a base
//! `kube::Config`. That config points at a closed port here, which is exactly
//! what these tests want — a request that reached a handler fails loudly on
//! connect rather than passing for the wrong reason, so a refusal asserted below
//! is a refusal the layers really made.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, WWW_AUTHENTICATE,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, Request, StatusCode};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use kopiur_ui::config::{
    self, AuthConfig, AuthMode, CacheLimits, DEFAULT_ADDR, DEFAULT_CLIENT_CACHE_SIZE,
    DEFAULT_GROUPS_SEPARATOR, DEFAULT_MAX_DOWNLOAD_BYTES, DEFAULT_MAX_EXEC_GLOBAL,
    DEFAULT_MAX_EXEC_PER_IDENTITY, DEFAULT_MAX_MANIFEST_BYTES, DEFAULT_MAX_SESSION_STARTS,
    DEFAULT_OPS_ADDR, DEFAULT_SAR_CACHE_SIZE, DEFAULT_SNAPSHOT_LIST_CAP, SessionLimits, UiConfig,
};
use kopiur_ui::metrics::UiMetrics;
use kopiur_ui::ops_listener::Readiness;
use kopiur_ui::{AppState, app, auth, browse, cache, static_files};
use kopiur_ui_model::problem::Problem;

const USER_HEADER: &str = "x-forwarded-user";
const GROUPS_HEADER: &str = "x-forwarded-groups";
const PROXY_TOKEN_HEADER: &str = "x-kopiur-proxy-token";
const PROXY_SECRET: &str = "s3cr3t";

/// The SPA's CSRF marker header, spelled here so a rename in `auth::csrf` shows
/// up as a failing wiring test rather than a silently unguarded mutation.
const REQUEST_HEADER: &str = "x-kopiur-request";
const REQUEST_HEADER_VALUE: &str = "1";

// --- fixtures ---------------------------------------------------------------

/// The production posture: a proxy asserts the identity, and it must prove it
/// really is the proxy.
fn header_mode_config() -> UiConfig {
    UiConfig {
        addr: DEFAULT_ADDR.parse().expect("default addr"),
        ops_addr: DEFAULT_OPS_ADDR.parse().expect("default ops addr"),
        auth: AuthConfig {
            mode: AuthMode::Headers {
                user: HeaderName::from_static(USER_HEADER),
                groups: Some(HeaderName::from_static(GROUPS_HEADER)),
                anonymous_fallback: None,
            },
            groups_separator: DEFAULT_GROUPS_SEPARATOR.to_string(),
            email_header: None,
            extra_keys: Vec::new(),
            allowed_groups: None,
            proxy_secret: Some(PROXY_SECRET.as_bytes().to_vec()),
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
        download_chunk_timeout: Duration::from_secs(60),
        manifest_max_bytes: DEFAULT_MAX_MANIFEST_BYTES,
        snapshot_list_cap: DEFAULT_SNAPSHOT_LIST_CAP,
        client_cache: CacheLimits {
            size: DEFAULT_CLIENT_CACHE_SIZE,
            ttl: Duration::from_secs(600),
        },
        sar_ttl: Duration::from_secs(60),
        sar_cache_size: DEFAULT_SAR_CACHE_SIZE,
        tls: None,
        cors_origins: Vec::new(),
    }
}

/// The state `app()` is built from, plus the metrics handle so a test can read
/// back what a request recorded.
fn state_from(cfg: UiConfig) -> (AppState, Arc<UiMetrics>) {
    let metrics = Arc::new(UiMetrics::new(Arc::new(
        kopiur_telemetry::MetricsProvider::new("kopiur-ui-router-test"),
    )));
    let state = AppState {
        metrics: Arc::clone(&metrics),
        readiness: Arc::new(Readiness::new(static_files::is_placeholder())),
        // A real `AuthState::new`, never `unconfigured()`: the placeholder is
        // fail-closed and 500s every request, which would mask every refusal
        // these tests exist to assert.
        auth: Arc::new(auth::AuthState::new(
            cfg.auth.clone(),
            kube::Config::new(
                "http://127.0.0.1:1/"
                    .parse()
                    .expect("a literal URL parses as a Uri"),
            ),
            cfg.client_cache.clone(),
        )),
        source: Arc::new(cache::Source::Impersonated),
        sessions: Arc::new(browse::session_pool::SessionPool::default()),
        cfg: Arc::new(cfg),
    };
    (state, metrics)
}

fn router() -> Router {
    app(state_from(header_mode_config()).0)
}

/// A request builder already carrying a valid proxy token and identity.
fn as_alice(method: &str, uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(USER_HEADER, "alice")
        .header(GROUPS_HEADER, "kopiur:viewer")
        .header(PROXY_TOKEN_HEADER, PROXY_SECRET)
}

fn anonymous(method: &str, uri: &str) -> axum::http::request::Builder {
    Request::builder().method(method).uri(uri)
}

/// Send one request through the real `app()` and keep everything about the
/// answer: a header nobody reads is a header that quietly disappears.
async fn send(request: Request<Body>) -> Answer {
    send_to(router(), request).await
}

async fn send_to(router: Router, request: Request<Body>) -> Answer {
    let response = router.oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    Answer {
        status,
        headers,
        body: bytes.to_vec(),
    }
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    /// The problem document, or a panic naming what came back instead — an
    /// assertion on a `type` that silently passed because the body was HTML
    /// would defeat the point of these tests.
    fn problem(&self) -> Problem {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "expected application/problem+json, got {} ({e}): {}",
                self.header(CONTENT_TYPE).unwrap_or("no content type"),
                String::from_utf8_lossy(&self.body),
            )
        })
    }

    fn header(&self, name: impl axum::http::header::AsHeaderName) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

// --- identity ---------------------------------------------------------------

#[tokio::test]
async fn a_request_with_no_identity_is_a_401_challenging_the_proxy() {
    let answer = send(
        anonymous("GET", "/api/v1/gates")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        answer.header(WWW_AUTHENTICATE),
        Some("Kopiur-Proxy"),
        "a browser must not be shown a credential dialog it can never satisfy, so the \
         challenge names the trust boundary rather than an HTTP auth scheme",
    );
    let problem = answer.problem();
    assert_eq!(problem.r#type, "urn:kopiur:problem:proxy-secret");
    assert_eq!(problem.instance.as_deref(), Some("/api/v1/gates"));
}

#[tokio::test]
async fn the_proxy_secret_is_checked_before_the_identity_headers_are_believed() {
    let answer = send(
        anonymous("GET", "/api/v1/gates")
            .header(USER_HEADER, "alice")
            .header(PROXY_TOKEN_HEADER, "wrong")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        answer.problem().r#type,
        "urn:kopiur:problem:proxy-secret",
        "in a cluster anything that can reach the Service can set the identity headers, so \
         they are only meaningful once the token has been checked",
    );
}

#[tokio::test]
async fn a_forbidden_principal_is_refused_before_any_handler_runs() {
    let answer = send(
        anonymous("GET", "/api/v1/gates")
            .header(USER_HEADER, "system:admin")
            .header(PROXY_TOKEN_HEADER, PROXY_SECRET)
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(
        answer.problem().r#type,
        "urn:kopiur:problem:forbidden-principal",
        "a proxy must never be able to make the UI impersonate a system: principal",
    );
}

/// The browse routes register ABSOLUTE paths and are therefore `merge`d at the
/// root, not nested — and a merged router inherits nothing from a nest point. If
/// the identity layer were applied to the nest point only, this request would
/// reach a handler and 500 on `CurrentIdentity` instead of 401ing.
#[tokio::test]
async fn a_browse_route_sits_behind_the_identity_layer_too() {
    let answer = send(
        anonymous("GET", "/api/v1/snapshots/media/nightly-1/tree")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(answer.problem().r#type, "urn:kopiur:problem:proxy-secret");
    assert_eq!(
        answer.header(CACHE_CONTROL),
        Some(config::API_CACHE_CONTROL),
        "the security headers must reach the merged whole, not just the nest point",
    );
}

// --- CSRF -------------------------------------------------------------------

#[tokio::test]
async fn a_mutation_without_the_spa_marker_header_is_refused() {
    let answer = send(
        as_alice("POST", "/api/v1/actions/suspend")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .expect("request"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(answer.problem().r#type, "urn:kopiur:problem:csrf");
    assert_eq!(
        answer.problem().instance.as_deref(),
        Some("/api/v1/actions/suspend")
    );
}

#[tokio::test]
async fn a_mutation_from_another_site_is_refused_even_with_the_marker_header() {
    let answer = send(
        as_alice("POST", "/api/v1/actions/suspend")
            .header(CONTENT_TYPE, "application/json")
            .header(REQUEST_HEADER, REQUEST_HEADER_VALUE)
            .header("sec-fetch-site", "cross-site")
            .body(Body::from("{}"))
            .expect("request"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(answer.problem().r#type, "urn:kopiur:problem:csrf");
}

// --- the API/SPA boundary ---------------------------------------------------

#[tokio::test]
async fn an_unknown_api_path_is_a_404_problem_not_the_spa() {
    // Deliberately WITHOUT identity headers. An honest 404 is what tells an
    // operator their bundle and their backend disagree; answering 401 here would
    // send them hunting a proxy problem that does not exist. That is the whole
    // reason the identity layer goes on with `route_layer`.
    let answer = send(
        anonymous("GET", "/api/v1/nope")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(
        answer.header(CONTENT_TYPE),
        Some("application/problem+json")
    );
    let problem = answer.problem();
    assert_eq!(
        problem.r#type, "urn:kopiur:problem:not-found",
        "one problem vocabulary across the crate, not an https: URL here and URNs everywhere \
         else",
    );
    assert_eq!(problem.instance.as_deref(), Some("/api/v1/nope"));
    assert!(problem.fix.contains("reload"), "{}", problem.fix);
}

#[tokio::test]
async fn an_unknown_browser_path_is_the_spa() {
    let answer = send(
        anonymous("GET", "/unknown-path")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        answer.header(CONTENT_TYPE),
        Some("text/html"),
        "a deep link is the client-side router's job, not a 404",
    );
}

// --- headers ----------------------------------------------------------------

#[tokio::test]
async fn every_api_response_carries_the_security_headers() {
    // Four paths across all three routers and both sides of the timeout split,
    // and both a refusal and a 404 — a header applied to one half only is
    // exactly the regression this catches.
    for uri in [
        "/api/v1/gates",
        "/api/v1/snapshots/media/nightly-1/tree",
        "/api/v1/snapshots/media/nightly-1/file?path=a.txt",
        "/api/v1/nope",
    ] {
        let answer = send(anonymous("GET", uri).body(Body::empty()).expect("request")).await;

        assert_eq!(answer.header(CACHE_CONTROL), Some("no-store"), "{uri}");
        assert_eq!(answer.header(X_FRAME_OPTIONS), Some("DENY"), "{uri}");
        assert_eq!(
            answer.header(X_CONTENT_TYPE_OPTIONS),
            Some("nosniff"),
            "{uri}"
        );
        let csp = answer
            .header(CONTENT_SECURITY_POLICY)
            .unwrap_or_else(|| panic!("{uri} must carry a CSP"));
        assert!(
            csp.contains("frame-ancestors 'none'"),
            "{uri} must forbid embedding: {csp}",
        );
        assert_eq!(
            answer.header(REFERRER_POLICY),
            Some("same-origin"),
            "{uri}: Kopiur URLs carry namespaces and object names",
        );
    }
}

/// `Cache-Control` is the one header that is `/api`-only. `no-store` on the
/// bundle would re-download the content-hashed assets on every navigation,
/// defeating the year-long `immutable` those hashes exist for.
#[tokio::test]
async fn only_cache_control_is_scoped_to_the_api() {
    let answer = send(
        anonymous("GET", "/unknown-path")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(answer.header(CACHE_CONTROL), Some("no-cache"));
    assert_eq!(
        answer.header(REFERRER_POLICY),
        Some("same-origin"),
        "the referrer policy is about the URL, not the body",
    );
}

/// The app **shell** is the document an attacker frames, so the framing and
/// sniffing headers must reach the HTML, not only the JSON. Scoping them to
/// `/api` would have hardened the half nobody frames and left the half they do.
///
/// `GET /` — the entry point a browser actually loads — plus a deep link, which
/// serves the same document through the SPA fallback.
#[tokio::test]
async fn the_spa_shell_carries_the_framing_and_sniffing_headers() {
    for uri in ["/", "/snapshots/prod/nightly-1"] {
        let answer = send(anonymous("GET", uri).body(Body::empty()).expect("request")).await;

        assert_eq!(answer.status, StatusCode::OK, "{uri}");
        assert_eq!(answer.header(CONTENT_TYPE), Some("text/html"), "{uri}");
        assert_eq!(answer.header(X_FRAME_OPTIONS), Some("DENY"), "{uri}");
        assert_eq!(
            answer.header(X_CONTENT_TYPE_OPTIONS),
            Some("nosniff"),
            "{uri}"
        );
        let csp = answer
            .header(CONTENT_SECURITY_POLICY)
            .unwrap_or_else(|| panic!("{uri} must carry a CSP"));
        assert!(
            csp.contains("frame-ancestors 'none'"),
            "{uri} must forbid embedding: {csp}",
        );
    }
}

// --- the timeout split ------------------------------------------------------

/// `…/file` must be mounted and must not be behind the request timeout, which is
/// why it lives in its own router. A merge that dropped it would turn every
/// download into a 404 — indistinguishable, from the SPA's side, from a stale
/// bundle.
#[tokio::test]
async fn the_download_route_is_mounted_outside_the_timeout() {
    let answer = send(
        as_alice("GET", "/api/v1/snapshots/media/nightly-1/file?path=a.txt")
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_ne!(
        answer.status,
        StatusCode::NOT_FOUND,
        "…/file must be reachable; here it fails for want of a cluster, not a route",
    );
    assert_ne!(
        answer.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "GET is the method it serves",
    );
}

/// `…/file` lives in its own un-timed router, merged separately, so its identity
/// coverage depends on an ordering a refactor could plausibly invert — merge the
/// untimed half *after* the identity layer (as it is merged after the timeout)
/// and the download silently loses it. `CurrentIdentity` then answers 500
/// `identity-missing` rather than failing to compile, and the header stack would
/// still decorate that 500, so nothing else here would notice.
#[tokio::test]
async fn an_anonymous_download_is_a_401_not_a_wiring_bug() {
    let answer = send(
        anonymous("GET", "/api/v1/snapshots/media/nightly-1/file?path=a.txt")
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    let problem = answer.problem();
    assert_eq!(
        problem.r#type, "urn:kopiur:problem:proxy-secret",
        "…/file must sit behind identity_middleware like every other API route",
    );
    assert_ne!(
        problem.r#type, "urn:kopiur:problem:identity-missing",
        "a 500 here would mean the route escaped the identity layer entirely",
    );
}

#[tokio::test]
async fn head_on_the_download_route_is_refused_with_an_allow_header() {
    let answer = send(
        as_alice("HEAD", "/api/v1/snapshots/media/nightly-1/file?path=a.txt")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        answer.header(axum::http::header::ALLOW),
        Some("GET"),
        "a 405 must name the method that would have worked",
    );
}

/// The retention plan is a third route under `/snapshots/{ns}/{name}` — beside
/// the API's own `GET` and the actions router's `DELETE`, and beside browse's
/// absolutely-registered `…/tree` and `…/session`. All five live in one matchit
/// tree, so "it is mounted and it resolves" is a wiring fact worth asserting:
/// a collision here would be a 404 or a 405, not a compile error.
#[tokio::test]
async fn the_retention_plan_route_is_mounted_beside_the_snapshot_detail() {
    let answer = send(
        as_alice("GET", "/api/v1/snapshots/media/nightly-1/retention")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_ne!(
        answer.status,
        StatusCode::NOT_FOUND,
        "…/retention must be reachable; here it fails for want of a cluster, not a route",
    );
    assert_ne!(
        answer.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "GET is the method it serves",
    );
    assert_eq!(
        answer.header(CACHE_CONTROL),
        Some(config::API_CACHE_CONTROL),
        "a new route inherits the shared API layers, not just the router it was added to",
    );
}

#[tokio::test]
async fn the_retention_plan_route_needs_an_identity_like_every_other_read() {
    let answer = send(
        anonymous("GET", "/api/v1/snapshots/media/nightly-1/retention")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(answer.problem().r#type, "urn:kopiur:problem:proxy-secret");
}

// --- one repository-kind vocabulary -----------------------------------------

/// `GET /repositories/{kind}/{name}` and `DELETE …/{kind}/{name}/session` are
/// the same segment in the same URL shape, and they used to disagree about it:
/// the read route took kebab `cluster-repository` case-sensitively, the session
/// route took `clusterrepository` and rejected the hyphen. One parser now backs
/// both, so every spelling must get past parsing on BOTH routes.
///
/// "Got past parsing" is asserted as *not* a 400 `invalid-path`/`invalid` and
/// not a 404: with no cluster to reach, a resolved segment fails deeper for want
/// of an apiserver, which is exactly the proof the routing and parsing worked.
#[tokio::test]
async fn both_repository_kind_routes_accept_the_same_spellings() {
    for spelling in [
        "repository",
        "Repository",
        "cluster-repository",
        "clusterrepository",
        "ClusterRepository",
    ] {
        let read = send(
            as_alice(
                "GET",
                &format!("/api/v1/repositories/{spelling}/nas?namespace=media"),
            )
            .body(Body::empty())
            .expect("request"),
        )
        .await;
        assert_ne!(read.status, StatusCode::NOT_FOUND, "GET {spelling}");
        assert_ne!(
            read.problem().r#type,
            "urn:kopiur:problem:invalid-path",
            "GET {spelling} must be a kind the read route reads",
        );

        let session = send(
            as_alice(
                "DELETE",
                // Both namespaces named, so the one refusal that is about the
                // URL rather than the kind — a ClusterRepository session with
                // no `sessionNamespace` — cannot mask the assertion.
                &format!(
                    "/api/v1/repositories/{spelling}/nas/session\
                     ?namespace=media&sessionNamespace=media"
                ),
            )
            .header(REQUEST_HEADER, REQUEST_HEADER_VALUE)
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .expect("request"),
        )
        .await;
        assert_ne!(session.status, StatusCode::NOT_FOUND, "DELETE {spelling}");
        assert_ne!(
            session.problem().r#type,
            "urn:kopiur:problem:invalid",
            "DELETE {spelling} must be a kind the session route reads",
        );
    }
}

// --- metrics ----------------------------------------------------------------

/// The `route` label must be the matched TEMPLATE. Labelling the raw path would
/// make the series cardinality the size of the fleet — and an unmatched path is
/// caller-controlled, so anyone who can reach the port could mint series at will.
#[tokio::test]
async fn requests_are_counted_by_matched_route_template_not_by_path() {
    let (state, metrics) = state_from(header_mode_config());
    let router = app(state);

    let answer = send_to(
        router,
        as_alice("GET", "/api/v1/snapshots/prod/nightly-1")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_ne!(
        answer.status,
        StatusCode::NOT_FOUND,
        "sanity: the route is mounted, so a counter was recorded for it",
    );

    let exposition = String::from_utf8(metrics.gather()).expect("exposition is UTF-8");
    assert!(
        exposition.contains(r#"route="/api/v1/snapshots/{namespace}/{name}""#),
        "the label must be the route template:\n{exposition}",
    );
    assert!(
        !exposition.contains("nightly-1"),
        "no object name may ever reach a label:\n{exposition}",
    );
    assert!(
        exposition.contains(r#"identity_source="trusted-headers""#),
        "a header-mode deployment must be labelled as one:\n{exposition}",
    );
}

/// A request the identity layer refused is still counted, and still labelled
/// with how this deployment establishes identity — that counter is how a silent
/// downgrade to the anonymous identity becomes visible.
#[tokio::test]
async fn a_refused_request_is_counted_too() {
    let (state, metrics) = state_from(header_mode_config());
    let router = app(state);

    let answer = send_to(
        router,
        anonymous("GET", "/api/v1/gates")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);

    let exposition = String::from_utf8(metrics.gather()).expect("exposition is UTF-8");
    assert!(
        exposition.contains(r#"route="/api/v1/gates""#),
        "{exposition}"
    );
    assert!(exposition.contains(r#"status="401""#), "{exposition}");
}

/// An unmatched `/api` path is counted under one fixed label, never the path
/// itself: the path is exactly the part a stranger chooses.
#[tokio::test]
async fn an_unmatched_api_path_is_counted_under_a_fixed_label() {
    let (state, metrics) = state_from(header_mode_config());
    let router = app(state);

    send_to(
        router,
        anonymous("GET", "/api/v1/whatever-i-like-1234")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    let exposition = String::from_utf8(metrics.gather()).expect("exposition is UTF-8");
    assert!(
        !exposition.contains("whatever-i-like-1234"),
        "a caller-chosen path must never become a label:\n{exposition}",
    );
    assert!(
        exposition.contains(r#"route="/api/{*rest}""#),
        "the catch-all's own template is the honest label:\n{exposition}",
    );
}

// --- bounds -----------------------------------------------------------------

/// The body limit is the one bound an authenticated caller could otherwise use
/// to make the process allocate at will. 64 KiB is orders of magnitude more than
/// the biggest legitimate body (a `Restore` request with a PVC template), so
/// anything over it is a bug or an attack, and either way it must be refused
/// before the JSON is parsed.
#[tokio::test]
async fn an_oversized_request_body_is_refused() {
    let answer = send(
        as_alice("POST", "/api/v1/actions/suspend")
            .header(CONTENT_TYPE, "application/json")
            .header(REQUEST_HEADER, REQUEST_HEADER_VALUE)
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(vec![b'x'; config::MAX_REQUEST_BODY_BYTES * 2]))
            .expect("request"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        answer.header(CONTENT_TYPE),
        Some("application/problem+json"),
        "even a refusal at the edge speaks the crate's one error shape",
    );
}

/// A body under the limit gets past it and reaches the handler's own parsing —
/// which is what proves the limit is a limit rather than a blanket refusal — and
/// the problem it comes back with names the path the CALLER asked for.
#[tokio::test]
async fn a_body_within_the_limit_reaches_the_handler() {
    let answer = send(
        as_alice("POST", "/api/v1/actions/suspend")
            .header(CONTENT_TYPE, "application/json")
            .header(REQUEST_HEADER, REQUEST_HEADER_VALUE)
            .header("sec-fetch-site", "same-origin")
            .body(Body::from(
                r#"{"kind":"nonsense","namespace":"p","name":"x","suspended":true}"#,
            ))
            .expect("request"),
    )
    .await;

    assert_ne!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_ne!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(
        answer.problem().instance.as_deref(),
        Some("/api/v1/actions/suspend"),
        "a problem from inside the /api/v1 nest must name the path the CALLER asked for, not \
         the one axum rewrote it to",
    );
}
