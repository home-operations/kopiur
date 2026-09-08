//! The ops listener: `/metrics`, `/healthz` and `/readyz` on their own port.
//!
//! A second listener rather than three more routes on the app port, for two
//! reasons. The app port is meant to be reachable only from the authenticating
//! proxy (the chart renders a NetworkPolicy that says so), while Prometheus and
//! the kubelet must still reach the probes; and `/metrics` must never sit behind
//! the identity middleware, which would either reject the scrape or — worse —
//! answer it as the anonymous identity.
//!
//! `main` starts this listener FIRST, before the kube client, the reflector
//! stores and the app server, so a UI that is failing to start is still
//! observable: the kubelet gets an answer from `/readyz` explaining what is not
//! ready instead of a connection refused.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::get;

use crate::metrics::UiMetrics;

/// The subsystems `/readyz` reports on.
///
/// Flipped as each one comes up (and, for impersonation, as it goes down again),
/// so readiness reflects what the process can actually do rather than merely
/// that it is running.
#[derive(Debug)]
pub struct Readiness {
    /// Whether the UI has a working kube client that can impersonate. False
    /// until the client is built and its first impersonated call succeeds:
    /// without it every request would fail, so the pod should not take traffic.
    pub impersonation_ok: AtomicBool,
    /// Whether every reflector store has synced. Always true when
    /// `KOPIUR_UI_CACHE` is off (there are no stores to sync). Serving from a
    /// half-filled cache would show a healthy cluster as an empty one.
    pub cache_ready: AtomicBool,
    /// Whether the embedded SPA is the `build.rs` placeholder rather than a real
    /// bundle. Fixed at compile time, hence not atomic.
    ///
    /// This makes the pod UNREADY, which is deliberate. A released image cannot
    /// hit it — `docker/Dockerfile.ui` sets `KOPIUR_UI_REQUIRE_WEB=1`, so a
    /// missing bundle fails the build instead — so if it ever fires in a cluster
    /// the image was built wrong, and the honest answer is "this pod cannot serve
    /// the UI", not a placeholder page behind a green probe.
    pub web_placeholder: bool,
}

impl Readiness {
    /// A process that has not brought anything up yet: not ready, for the
    /// reasons `/readyz` will list.
    pub fn new(web_placeholder: bool) -> Self {
        Self {
            impersonation_ok: AtomicBool::new(false),
            cache_ready: AtomicBool::new(false),
            web_placeholder,
        }
    }

    /// Every reason this process is not ready, newest concern first. Empty means
    /// ready.
    ///
    /// The strings are stable tokens, not prose: they go straight into the
    /// `/readyz` body, and an operator (or an e2e assertion) greps for them.
    pub fn reasons(&self) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if !self.impersonation_ok.load(Ordering::Relaxed) {
            reasons.push("impersonation-unavailable");
        }
        if !self.cache_ready.load(Ordering::Relaxed) {
            reasons.push("cache-not-ready");
        }
        if self.web_placeholder {
            reasons.push("placeholder-web");
        }
        reasons
    }
}

/// Serve `/metrics`, `/healthz` and `/readyz` on `addr` until the process ends.
///
/// `/healthz` is liveness: it answers 200 as long as the process can serve HTTP
/// at all, because restarting a UI that is merely waiting on the apiserver fixes
/// nothing. `/readyz` is the one that gates traffic.
pub async fn serve_ops(
    addr: SocketAddr,
    metrics: Arc<UiMetrics>,
    readiness: Arc<Readiness>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let app = axum::Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(OpsState { metrics, readiness });

    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
        format!(
            "binding the kopiur-ui ops server to {addr}; if this host has IPv6 disabled a \
             `[::]` bind fails — set KOPIUR_UI_OPS_ADDR=0.0.0.0:{} (via the chart's \
             ui.extraEnv)",
            addr.port()
        )
    })?;
    tracing::info!(%addr, "ops server listening (/metrics, /healthz, /readyz)");
    axum::serve(listener, app).await?;
    Ok(())
}

/// What the three ops handlers share.
#[derive(Clone)]
struct OpsState {
    metrics: Arc<UiMetrics>,
    readiness: Arc<Readiness>,
}

async fn metrics_handler(State(state): State<OpsState>) -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.gather(),
    )
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(state): State<OpsState>) -> impl IntoResponse {
    let reasons = state.readiness.reasons();
    if reasons.is_empty() {
        return (StatusCode::OK, "ok\n".to_string());
    }
    // Plain text, one reason per line: this body is read by a human running
    // `kubectl describe pod` or `curl`, and it is the only place that says WHY.
    (
        StatusCode::SERVICE_UNAVAILABLE,
        format!("not ready: {}\n", reasons.join(", ")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_process_is_not_ready_and_says_why() {
        let readiness = Readiness::new(false);
        assert_eq!(
            readiness.reasons(),
            vec!["impersonation-unavailable", "cache-not-ready"]
        );
    }

    #[test]
    fn readiness_clears_only_when_every_subsystem_is_up() {
        let readiness = Readiness::new(false);
        readiness.impersonation_ok.store(true, Ordering::Relaxed);
        assert_eq!(readiness.reasons(), vec!["cache-not-ready"]);
        readiness.cache_ready.store(true, Ordering::Relaxed);
        assert!(readiness.reasons().is_empty());
    }

    #[test]
    fn a_placeholder_web_bundle_keeps_the_pod_unready() {
        // A backend-only build serves a stand-in page. That is fine for
        // development and must never be fine in a cluster, so it is reported
        // even when everything else is up.
        let readiness = Readiness::new(true);
        readiness.impersonation_ok.store(true, Ordering::Relaxed);
        readiness.cache_ready.store(true, Ordering::Relaxed);
        assert_eq!(readiness.reasons(), vec!["placeholder-web"]);
    }
}
