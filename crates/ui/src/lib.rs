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
pub mod static_files;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Response, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::any;

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
    /// Where reads come from: the reflector cache or a live impersonated call.
    pub source: Arc<cache::Source>,
    /// Browse-session start and exec bounds.
    pub sessions: Arc<browse::session_pool::SessionPool>,
}

/// The app router: the SPA plus `/api/v1`.
///
/// Task 8 mounts the real `/api/v1` routes. Until then the API namespace is
/// explicitly claimed and answers 404 with the same `application/problem+json`
/// shape every other endpoint uses — an unmounted route must never fall through
/// to [`static_files::spa_fallback`], which would answer an API call with 200 and
/// an HTML page and leave the SPA parsing `<!doctype html>` as JSON.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/api", any(api_not_found))
        .route("/api/{*rest}", any(api_not_found))
        .fallback(static_files::spa_fallback)
        .with_state(state)
}

/// 404 for an `/api` path that no handler claims.
async fn api_not_found(uri: Uri) -> Response<Body> {
    problem_response(
        StatusCode::NOT_FOUND,
        kopiur_ui_model::problem::Problem {
            r#type: "https://kopiur.dev/problems/not-found".to_string(),
            title: "Endpoint not found".to_string(),
            status: StatusCode::NOT_FOUND.as_u16(),
            detail: format!("no kopiur-ui endpoint serves {}", uri.path()),
            what: format!(
                "The UI asked for {}, which this backend does not serve.",
                uri.path()
            ),
            why: "The path is not part of the kopiur-ui API surface — usually a stale SPA \
                  bundle calling an endpoint a newer or older backend has, or a typo in a \
                  hand-made request."
                .to_string(),
            fix: "Reload the page to pick up the bundle this backend ships, and make sure the \
                  kopiur-ui image and the operator come from the same release."
                .to_string(),
            instance: Some(uri.path().to_string()),
            kube_reason: None,
        },
    )
}

/// Render a [`kopiur_ui_model::problem::Problem`] as RFC 9457
/// `application/problem+json`.
///
/// Serialized by hand rather than through `axum::Json` so the content type is
/// unambiguously the problem type — the SPA switches on it to decide whether a
/// body is an error.
fn problem_response(
    status: StatusCode,
    problem: kopiur_ui_model::problem::Problem,
) -> Response<Body> {
    match serde_json::to_vec(&problem) {
        Ok(body) => (status, [(CONTENT_TYPE, "application/problem+json")], body).into_response(),
        // A Problem is plain owned strings, so this cannot fail; degrade to
        // plain text rather than panic inside a handler.
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize a Problem response");
            (status, problem.detail).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body as ReqBody;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    fn test_state() -> AppState {
        let provider = Arc::new(kopiur_telemetry::MetricsProvider::new("kopiur-ui-test"));
        AppState {
            cfg: Arc::new(test_config()),
            metrics: Arc::new(UiMetrics::new(provider)),
            readiness: Arc::new(Readiness::new(static_files::is_placeholder())),
            auth: Arc::new(auth::AuthState::default()),
            source: Arc::new(cache::Source::Impersonated),
            sessions: Arc::new(browse::session_pool::SessionPool::default()),
        }
    }

    fn test_config() -> UiConfig {
        use crate::config::*;
        UiConfig {
            addr: DEFAULT_ADDR.parse().unwrap(),
            ops_addr: DEFAULT_OPS_ADDR.parse().unwrap(),
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
            snapshot_list_cap: DEFAULT_SNAPSHOT_LIST_CAP,
            client_cache: CacheLimits {
                size: DEFAULT_CLIENT_CACHE_SIZE,
                ttl: std::time::Duration::from_secs(600),
            },
            sar_ttl: std::time::Duration::from_secs(60),
            tls: None,
            cors_origins: Vec::new(),
        }
    }

    #[tokio::test]
    async fn an_unmounted_api_path_is_a_problem_json_404_not_the_spa() {
        let response = app(test_state())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/nothing-here")
                    .body(ReqBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .map(|v| v.to_str().unwrap()),
            Some("application/problem+json"),
            "an API call must never be answered with the SPA's HTML"
        );

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let problem: kopiur_ui_model::problem::Problem = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem.status, 404);
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
                Request::builder()
                    .uri("/snapshots/prod/nightly-1")
                    .body(ReqBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .map(|v| v.to_str().unwrap()),
            Some("text/html"),
            "deep links are handled by the client-side router, not by a 404"
        );
    }
}
