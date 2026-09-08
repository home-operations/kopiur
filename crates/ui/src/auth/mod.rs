//! Establishing who the caller is, and speaking to the apiserver as them.
//!
//! `kopiur-ui` authorizes nothing itself. It resolves an [`identity::Identity`]
//! from trusted proxy headers (or the configured anonymous identity), verifies
//! that the request really arrived through the proxy ([`proxy_secret`]), and then
//! issues every apiserver call *impersonating* that identity ([`impersonate`]) —
//! so RBAC is enforced exactly where it always was, in the apiserver, and a bug
//! in this crate cannot grant a permission the caller does not have.
//!
//! # The order in [`identity_middleware`] is the security argument
//!
//! 1. [`redact::strip_sensitive`] — the browser's `Cookie`/`Authorization` and
//!    any forwarded upstream token are removed *first*, so nothing downstream,
//!    including the tracing layer and every error path, can record one.
//! 2. [`proxy_secret::verify_proxy_secret`] — before the identity headers are
//!    read, not after. In header mode they are only meaningful coming from the
//!    proxy, and in a cluster anything that can reach the Service can set them.
//! 3. [`identity::extract_identity`] — the deny-list, the bounds, and the
//!    deliberate blindness to inbound `Impersonate-*`.
//! 4. The resolved identity goes into the request extensions, where
//!    [`CurrentIdentity`] is the only way to read it.

pub mod csrf;
pub mod identity;
pub mod impersonate;
pub mod proxy_secret;
pub mod redact;

use std::time::Duration;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::header::{CONTENT_LENGTH, HeaderMap, TRANSFER_ENCODING};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use kopiur_ui_model::identity::IdentitySource;

use crate::AppState;
use crate::api::problem::{ApiError, problem};
use crate::config::{AuthConfig, AuthMode, CacheLimits};

use self::identity::Identity;
use self::impersonate::ClientCache;

/// Route label on `kopiur_ui_requests_total` for requests counted here.
///
/// The counter's `route` label is a `&'static str` and the middleware only knows
/// the concrete path, which carries namespaces and object names — a label the
/// size of the fleet. The API mount point is the honest coarse answer; per-route
/// labels belong to the router that knows its own templates.
const REQUEST_ROUTE: &str = "/api";

/// Everything the identity middleware needs at request time: the resolved
/// [`AuthConfig`] and the bounded per-identity impersonating client cache.
#[derive(Debug)]
pub struct AuthState {
    /// How a request's identity is established, and what may be impersonated.
    pub cfg: AuthConfig,
    /// One `kube::Client` per distinct caller, bounded and idle-expiring.
    pub clients: ClientCache,
    /// Whether `cfg` came from a resolved configuration or is the placeholder
    /// [`AuthState::unconfigured`] left. Private, and the reason there is no
    /// `Default`: a state nobody configured must refuse, not serve.
    wired: bool,
}

impl AuthState {
    /// Build the auth state from the resolved config and the base `kube::Config`
    /// (in-cluster, or inferred) that carries the UI's own ServiceAccount.
    pub fn new(cfg: AuthConfig, base: kube::Config, limits: CacheLimits) -> Self {
        let extra_keys = cfg.extra_keys.clone();
        Self {
            clients: ClientCache::new(base, limits, extra_keys),
            cfg,
            wired: true,
        }
    }

    /// A placeholder that **cannot serve**: every identity resolution returns
    /// [`identity::AuthError::NotWired`].
    ///
    /// `AppState` has to be constructible before `main` has resolved a
    /// configuration, and tests that never touch auth need *something* there.
    /// This is deliberately not a `Default` and deliberately not an anonymous
    /// identity: a placeholder that quietly resolved every caller to some
    /// invented user would be a fail-open — requests would be served, as a
    /// subject nobody chose, if a wiring change ever left it in place. Refusing
    /// with a 500 makes that mistake loud on the first request instead.
    pub fn unconfigured() -> Self {
        let mut state = Self::new(
            AuthConfig {
                mode: AuthMode::AnonymousOnly(crate::config::AnonymousIdentity {
                    user: "kopiur-ui-unconfigured".to_string(),
                    groups: Vec::new(),
                }),
                groups_separator: crate::config::DEFAULT_GROUPS_SEPARATOR.to_string(),
                email_header: None,
                extra_keys: Vec::new(),
                allowed_groups: None,
                proxy_secret: None,
            },
            kube::Config::new(
                "http://127.0.0.1:1/"
                    .parse()
                    .expect("a literal URL parses as a Uri"),
            ),
            CacheLimits {
                size: crate::config::DEFAULT_CLIENT_CACHE_SIZE,
                ttl: Duration::from_secs(600),
            },
        );
        state.wired = false;
        state
    }

    /// The identity this request runs as, or the reason it has none.
    pub fn identify(&self, headers: &HeaderMap) -> Result<Identity, identity::AuthError> {
        if !self.wired {
            return Err(identity::AuthError::NotWired);
        }
        // Anonymous-only mode has no identity header to forge, so it has no trust
        // boundary to protect and `resolve()` never gives it a secret. Matched
        // exhaustively so a third mode cannot skip the check by accident.
        match &self.cfg.mode {
            AuthMode::Headers { .. } => {
                proxy_secret::verify_proxy_secret(headers, self.cfg.proxy_secret.as_deref())?;
            }
            AuthMode::AnonymousOnly(_) => {}
        }
        identity::extract_identity(headers, &self.cfg)
    }

    /// How this deployment establishes identity, for the
    /// `kopiur_ui_requests_total{identity_source}` label on a request that failed
    /// before an [`Identity`] existed.
    fn mode_source(&self) -> IdentitySource {
        match &self.cfg.mode {
            AuthMode::Headers { .. } => IdentitySource::TrustedHeaders,
            AuthMode::AnonymousOnly(_) => IdentitySource::Anonymous,
        }
    }
}

/// Resolve the caller's identity and publish it to the handlers.
///
/// Mounted with `axum::middleware::from_fn_with_state` over everything under
/// `/api`. A request that reaches a handler has an [`Identity`] in its
/// extensions; one that does not never reaches a handler at all.
pub async fn identity_middleware(
    State(app): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    redact::strip_sensitive(req.headers_mut());

    let identity = match app.auth.identify(req.headers()) {
        Ok(identity) => identity,
        Err(error) => {
            let response = ApiError::from(error)
                .with_instance(req.uri().path().to_string())
                .into_response();
            app.metrics.inc_request(
                REQUEST_ROUTE,
                response.status().as_u16(),
                &app.auth.mode_source(),
            );
            return response;
        }
    };

    // The proxy's shared secret has done its one job. It is a long-lived
    // credential for the trust boundary itself, so it must not survive into a
    // handler, a `TraceLayer`, or an error path — the same argument as
    // [`redact::STRIPPED_HEADERS`], applied after the check rather than before it.
    req.headers_mut().remove(proxy_secret::PROXY_TOKEN_HEADER);

    let source = identity.source.clone();
    tracing::debug!(
        user = %identity.user,
        groups = identity.groups.len(),
        source = ?source,
        path = %req.uri().path(),
        "request identified"
    );
    req.extensions_mut().insert(identity);

    let response = next.run(req).await;
    app.metrics
        .inc_request(REQUEST_ROUTE, response.status().as_u16(), &source);
    app.metrics.set_identity_cache_size(app.auth.clients.len());
    response
}

/// The caller, extracted from what [`identity_middleware`] put in the request.
///
/// Reading the identity only through this extractor is what keeps the
/// impersonation guarantee auditable: a handler cannot invent one, and cannot
/// read the headers it came from.
#[derive(Debug, Clone)]
pub struct CurrentIdentity(pub Identity);

impl FromRequestParts<AppState> for CurrentIdentity {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match parts.extensions.get::<Identity>() {
            Some(identity) => Ok(Self(identity.clone())),
            // Only reachable if a route was mounted outside the middleware. It is
            // a wiring bug, not a caller error, and answering 500 rather than
            // falling back to *some* identity is the point: a handler must never
            // run as anyone it was not told to run as.
            None => Err(problem(
                500,
                "identity-missing",
                "kopiur-ui could not determine which identity to run this request as.",
                "The endpoint was mounted outside the identity middleware, so no caller was \
                 ever resolved for it. This is a bug in kopiur-ui, not in the request.",
                "report this at https://github.com/home-operations/kopiur/issues, naming the \
                 URL you requested",
            )
            .with_instance(parts.uri.path().to_string())),
        }
    }
}

/// Refuse a mutating request that did not come from the kopiur UI.
///
/// Mounted with `axum::middleware::from_fn` over the routers that write; see
/// [`csrf`] for why the marker header is the load-bearing check and the rest is
/// defence in depth.
///
/// Safe methods pass untouched. CSRF is about a request that *changes* something,
/// and a `GET` that a cross-site page triggers reads nothing back to it (the SPA's
/// responses are same-origin-only); gating them would break `GET …/file`, which
/// the browser performs as a navigation and which
/// [`csrf::require_same_site_navigation`] guards instead.
pub async fn mutation_guard(req: Request, next: Next) -> Response {
    if req.method().is_safe() {
        return next.run(req).await;
    }
    // HTTP/2 sends no `Host` header; hyper puts its `:authority` on the URI, so
    // the origin check needs both to have something to compare against.
    let authority = req.uri().authority().map(|a| a.as_str().to_string());
    if let Err(error) =
        csrf::require_mutation_headers(req.headers(), has_body(req.headers()), authority.as_deref())
    {
        return ApiError::from(error)
            .with_instance(req.uri().path().to_string())
            .into_response();
    }
    next.run(req).await
}

/// Whether the request carries a payload whose `Content-Type` must be checked.
///
/// A declared `Content-Type` counts even without a length, because a body that is
/// mislabelled is exactly the case the check exists for; a chunked body has no
/// `Content-Length` at all.
fn has_body(headers: &HeaderMap) -> bool {
    if headers.contains_key(axum::http::header::CONTENT_TYPE) {
        return true;
    }
    if headers.contains_key(TRANSFER_ENCODING) {
        return true;
    }
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|len| len > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderName, HeaderValue, Request as HttpRequest, StatusCode};
    use axum::routing::{get, post};
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    use crate::config::{AnonymousIdentity, DEFAULT_GROUPS_SEPARATOR, UiConfig};
    use kopiur_ui_model::problem::Problem;

    const USER_HEADER: &str = "x-forwarded-user";
    const GROUPS_HEADER: &str = "x-forwarded-groups";

    fn header_auth(proxy_secret: Option<&[u8]>) -> AuthConfig {
        AuthConfig {
            mode: AuthMode::Headers {
                user: HeaderName::from_static(USER_HEADER),
                groups: Some(HeaderName::from_static(GROUPS_HEADER)),
                anonymous_fallback: None,
            },
            groups_separator: DEFAULT_GROUPS_SEPARATOR.to_string(),
            email_header: None,
            extra_keys: Vec::new(),
            allowed_groups: None,
            proxy_secret: proxy_secret.map(<[u8]>::to_vec),
        }
    }

    fn anonymous_auth() -> AuthConfig {
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

    fn state(auth: AuthConfig) -> AppState {
        state_with(auth.clone(), || {
            AuthState::new(
                auth.clone(),
                kube::Config::new("http://127.0.0.1:1/".parse().expect("test url")),
                CacheLimits {
                    size: 8,
                    ttl: Duration::from_secs(600),
                },
            )
        })
    }

    fn state_with(auth: AuthConfig, build: impl Fn() -> AuthState) -> AppState {
        let provider = Arc::new(kopiur_telemetry::MetricsProvider::new("kopiur-ui-test"));
        let mut cfg = base_config();
        cfg.auth = auth;
        AppState {
            cfg: Arc::new(cfg),
            metrics: Arc::new(crate::metrics::UiMetrics::new(provider)),
            readiness: Arc::new(crate::ops_listener::Readiness::new(
                crate::static_files::is_placeholder(),
            )),
            auth: Arc::new(build()),
            source: Arc::new(crate::cache::Source::Impersonated),
            sessions: Arc::new(crate::browse::session_pool::SessionPool::default()),
        }
    }

    fn base_config() -> UiConfig {
        use crate::config::*;
        UiConfig {
            addr: DEFAULT_ADDR.parse().expect("default addr"),
            ops_addr: DEFAULT_OPS_ADDR.parse().expect("default ops addr"),
            auth: anonymous_auth(),
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

    /// A router that echoes back what the middleware resolved, so a test can
    /// assert on the identity a handler actually sees.
    fn app(auth: AuthConfig) -> Router {
        router(state(auth))
    }

    /// The same router over the placeholder auth state `main` starts with.
    fn unwired_app() -> Router {
        router(state_with(anonymous_auth(), AuthState::unconfigured))
    }

    fn router(state: AppState) -> Router {
        Router::new()
            .route("/api/v1/whoami", get(whoami))
            .route("/api/v1/write", post(write))
            .layer(axum::middleware::from_fn(mutation_guard))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                identity_middleware,
            ))
            .with_state(state)
    }

    /// `user|groups|whether a Cookie survived|whether the proxy token survived`.
    async fn whoami(CurrentIdentity(id): CurrentIdentity, headers: HeaderMap) -> String {
        format!(
            "{}|{}|{}|{}",
            id.user,
            id.groups.join(","),
            headers.contains_key("cookie"),
            headers.contains_key(proxy_secret::PROXY_TOKEN_HEADER)
        )
    }

    async fn write() -> &'static str {
        "written"
    }

    fn get_request(pairs: &[(&str, &str)]) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder().uri("/api/v1/whoami");
        for (name, value) in pairs {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).expect("test request")
    }

    async fn body_text(response: Response) -> String {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("utf-8 body")
    }

    async fn body_problem(response: Response) -> Problem {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("problem+json")
    }

    #[tokio::test]
    async fn a_request_with_no_identity_is_a_problem_json_401() {
        let response = app(header_auth(None))
            .oneshot(get_request(&[]))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::WWW_AUTHENTICATE)
                .unwrap(),
            "Kopiur-Proxy"
        );
        let problem = body_problem(response).await;
        assert_eq!(problem.r#type, "urn:kopiur:problem:no-identity");
        assert_eq!(problem.instance.as_deref(), Some("/api/v1/whoami"));
        assert!(problem.fix.contains(USER_HEADER), "{}", problem.fix);
    }

    #[tokio::test]
    async fn the_proxy_secret_is_checked_before_the_identity_headers_are_believed() {
        let auth = header_auth(Some(b"s3cr3t"));

        for (headers, why) in [
            (vec![(USER_HEADER, "alice")], "no token at all"),
            (
                vec![(USER_HEADER, "alice"), ("x-kopiur-proxy-token", "wrong")],
                "the wrong token",
            ),
        ] {
            let response = app(auth.clone())
                .oneshot(get_request(&headers))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{why}");
            let problem = body_problem(response).await;
            assert_eq!(problem.r#type, "urn:kopiur:problem:proxy-secret", "{why}");
        }

        let response = app(auth)
            .oneshot(get_request(&[
                (USER_HEADER, "alice"),
                ("x-kopiur-proxy-token", "s3cr3t"),
            ]))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn the_handler_sees_the_resolved_identity_and_no_credentials() {
        let response = app(header_auth(Some(b"s3cr3t")))
            .oneshot(get_request(&[
                (USER_HEADER, "alice"),
                (GROUPS_HEADER, "ops,dev"),
                ("x-kopiur-proxy-token", "s3cr3t"),
                ("cookie", "session=abc"),
                ("authorization", "Bearer nope"),
                ("proxy-authorization", "Basic nope"),
                ("x-forwarded-access-token", "nope"),
                ("x-forwarded-authorization", "nope"),
            ]))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        let fields: Vec<&str> = body.split('|').collect();
        assert_eq!(fields[0], "alice");
        assert_eq!(fields[1], "dev,ops,system:authenticated");
        assert_eq!(
            fields[2], "false",
            "the handler must not be able to read a Cookie: {body}"
        );
        assert_eq!(
            fields[3], "false",
            "the proxy shared secret is a credential for the trust boundary and must not \
             survive the check that consumed it: {body}"
        );
    }

    #[tokio::test]
    async fn a_forbidden_principal_is_a_403_before_any_handler_runs() {
        let response = app(header_auth(None))
            .oneshot(get_request(&[
                (USER_HEADER, "alice"),
                (GROUPS_HEADER, "system:masters"),
            ]))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let problem = body_problem(response).await;
        assert_eq!(problem.r#type, "urn:kopiur:problem:forbidden-principal");
    }

    #[tokio::test]
    async fn anonymous_only_mode_needs_neither_headers_nor_a_secret() {
        let response = app(anonymous_auth())
            .oneshot(get_request(&[]))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.starts_with("viewer|"));
    }

    #[tokio::test]
    async fn a_mutation_without_the_marker_header_is_refused() {
        let response = app(anonymous_auth())
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/v1/write")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .expect("test request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let problem = body_problem(response).await;
        assert_eq!(problem.r#type, "urn:kopiur:problem:csrf");
        assert_eq!(problem.instance.as_deref(), Some("/api/v1/write"));
    }

    #[tokio::test]
    async fn a_mutation_from_the_spa_is_allowed() {
        let response = app(anonymous_auth())
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/v1/write")
                    .header("content-type", "application/json")
                    .header(csrf::REQUEST_HEADER, csrf::REQUEST_HEADER_VALUE)
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from("{}"))
                    .expect("test request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_mutation_with_a_form_body_is_refused_even_with_the_marker_header() {
        let response = app(anonymous_auth())
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/v1/write")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header(csrf::REQUEST_HEADER, csrf::REQUEST_HEADER_VALUE)
                    .body(Body::from("a=1"))
                    .expect("test request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_body_is_detected_from_any_of_its_framings() {
        let mut headers = HeaderMap::new();
        assert!(!has_body(&headers), "a bare DELETE carries nothing");

        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
        assert!(!has_body(&headers));
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("2"));
        assert!(has_body(&headers));

        let mut headers = HeaderMap::new();
        headers.insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        assert!(has_body(&headers), "a chunked body has no length");

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        );
        assert!(
            has_body(&headers),
            "a declared content type is checked even without a length"
        );
    }

    #[tokio::test]
    async fn the_unconfigured_placeholder_refuses_every_request_instead_of_inventing_an_identity() {
        let response = unwired_app()
            .oneshot(get_request(&[(USER_HEADER, "alice")]))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let problem = body_problem(response).await;
        assert_eq!(problem.r#type, "urn:kopiur:problem:not-wired");
        assert!(problem.fix.contains("report it"), "{}", problem.fix);
    }

    #[tokio::test]
    async fn the_unconfigured_placeholder_refuses_an_anonymous_request_too() {
        // The old `Default` served this one as an invented anonymous user.
        let response = unwired_app()
            .oneshot(get_request(&[]))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body_problem(response).await.r#type,
            "urn:kopiur:problem:not-wired"
        );
    }

    #[tokio::test]
    async fn a_wired_state_is_never_not_wired() {
        assert!(
            AuthState::new(
                anonymous_auth(),
                kube::Config::new("http://127.0.0.1:1/".parse().expect("test url")),
                CacheLimits {
                    size: 1,
                    ttl: Duration::from_secs(1)
                },
            )
            .identify(&HeaderMap::new())
            .is_ok()
        );
    }

    #[test]
    fn the_metrics_label_reflects_how_the_deployment_identifies_callers() {
        assert_eq!(
            AuthState::new(
                header_auth(None),
                kube::Config::new("http://127.0.0.1:1/".parse().expect("test url")),
                CacheLimits {
                    size: 1,
                    ttl: Duration::from_secs(1)
                },
            )
            .mode_source(),
            IdentitySource::TrustedHeaders
        );
        assert_eq!(
            AuthState::unconfigured().mode_source(),
            IdentitySource::Anonymous
        );
    }
}
