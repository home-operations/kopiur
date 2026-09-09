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
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::get;

use crate::metrics::UiMetrics;

/// What the reflector cache is doing, as `/readyz` reports it.
///
/// A closed enum rather than a second `AtomicBool`, because "not ready" covers
/// four situations an operator has to tell apart and would otherwise have to
/// guess between: still listing (wait), never listed within the budget (check
/// the UI's own RBAC and whether the CRDs are installed), a watch that died
/// (look at the logs), and no cache configured at all (nothing to wait for).
/// Every conversion below is an exhaustive `match`, so a new state cannot be
/// added without deciding both how it is stored and what `/readyz` calls it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheState {
    /// `KOPIUR_UI_CACHE` is off: reads go straight to the apiserver under the
    /// caller's identity, so there is nothing to be ready for.
    Disabled,
    /// The stores are still completing their initial list.
    Syncing,
    /// Every store has completed its initial list.
    Synced,
    /// The stores did not all sync within [`crate::config::CACHE_SYNC_TIMEOUT`].
    SyncTimedOut,
    /// A reflector task ended, so at least one store has stopped refreshing and
    /// is now frozen at whatever it last held.
    ///
    /// Reachable at **any** point in the process's life, not only before the
    /// first sync: `cache::stores`'s supervisor keeps the reflector handles and
    /// publishes this the moment one of them finishes, which is what stops a
    /// watch that gave up an hour in from leaving `/readyz` answering `ok` over a
    /// frozen store. Also covers the two ways the machinery itself can go away —
    /// a writer dropped before the first list, and the supervisor task ending.
    ///
    /// Terminal: a reflector that gave up does not restart itself, so this never
    /// returns to [`CacheState::Synced`] without a new process.
    WatchEnded,
}

impl CacheState {
    /// The `/readyz` token for a state that blocks readiness, or `None` for one
    /// that does not.
    ///
    /// Stable tokens, not prose: they go straight into the response body and an
    /// operator (or an e2e assertion) greps for them.
    fn blocking_reason(self) -> Option<&'static str> {
        match self {
            Self::Disabled | Self::Synced => None,
            Self::Syncing => Some("cache-not-ready"),
            Self::SyncTimedOut => Some("cache-sync-timed-out"),
            Self::WatchEnded => Some("cache-watch-ended"),
        }
    }

    /// Round-trip through the atomic's storage.
    fn as_u8(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::Syncing => 1,
            Self::Synced => 2,
            Self::SyncTimedOut => 3,
            Self::WatchEnded => 4,
        }
    }

    /// The inverse of [`CacheState::as_u8`].
    ///
    /// Only ever fed a value [`CacheState::as_u8`] wrote, so the fallback is
    /// unreachable; it is `Syncing` rather than a panic because reporting "not
    /// ready yet" is the safe answer to a value nobody understands.
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Disabled,
            2 => Self::Synced,
            3 => Self::SyncTimedOut,
            4 => Self::WatchEnded,
            _ => Self::Syncing,
        }
    }
}

/// The subsystems `/readyz` reports on.
///
/// Flipped as each one comes up (and, for impersonation, as it goes down again),
/// so readiness reflects what the process can actually do rather than merely
/// that it is running.
#[derive(Debug)]
pub struct Readiness {
    /// Whether the UI has a working kube client that can impersonate. False
    /// until the startup `SelfSubjectAccessReview` confirms the UI's own
    /// ServiceAccount may `impersonate` users and groups: without that every
    /// request would fail, so the pod must not take traffic.
    pub impersonation_ok: AtomicBool,
    /// What the reflector cache is doing, as a [`CacheState`]. Read and written
    /// through [`Readiness::cache`]/[`Readiness::set_cache`].
    cache: AtomicU8,
    /// Whether the embedded SPA is the `build.rs` placeholder rather than a real
    /// bundle. Fixed at compile time, hence not atomic.
    ///
    /// This does **not** make the pod unready. A placeholder bundle is a valid
    /// development state — `cargo run -p kopiur-ui` in a checkout with no
    /// `web/dist` is how the API half is exercised, and refusing traffic there
    /// would mean the backend could never be reached at all. A released image
    /// cannot hit it either way: `docker/Dockerfile.ui` builds with
    /// `KOPIUR_UI_REQUIRE_WEB=1`, so a missing bundle fails the *build*. It is
    /// reported on `/readyz` as a note beside the `ok`, and logged loudly at
    /// startup, so it is visible without being fatal.
    pub web_placeholder: bool,
}

impl Readiness {
    /// A process that has not brought anything up yet: not ready, for the
    /// reasons `/readyz` will list.
    ///
    /// The cache starts [`CacheState::Syncing`]; `main` sets it to
    /// [`CacheState::Disabled`] immediately when `KOPIUR_UI_CACHE` is off, and
    /// otherwise moves it as the stores report in. Starting at `Syncing` rather
    /// than `Disabled` is deliberate: a wiring change that forgot to set it
    /// leaves the pod unready and loudly says why, instead of reporting ready
    /// over nine empty stores.
    pub fn new(web_placeholder: bool) -> Self {
        Self {
            impersonation_ok: AtomicBool::new(false),
            cache: AtomicU8::new(CacheState::Syncing.as_u8()),
            web_placeholder,
        }
    }

    /// What the reflector cache is currently doing.
    pub fn cache(&self) -> CacheState {
        CacheState::from_u8(self.cache.load(Ordering::Relaxed))
    }

    /// Record what the reflector cache is doing.
    pub fn set_cache(&self, state: CacheState) {
        self.cache.store(state.as_u8(), Ordering::Relaxed);
    }

    /// Every reason this process is **not ready**, most fundamental first. Empty
    /// means ready.
    ///
    /// The strings are stable tokens, not prose: they go straight into the
    /// `/readyz` body, and an operator (or an e2e assertion) greps for them.
    pub fn reasons(&self) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if !self.impersonation_ok.load(Ordering::Relaxed) {
            reasons.push("impersonation-unavailable");
        }
        reasons.extend(self.cache().blocking_reason());
        reasons
    }

    /// Things worth saying about a process that IS ready. Empty means there is
    /// nothing to add.
    ///
    /// Separate from [`Readiness::reasons`] because the two answer different
    /// questions and only one of them gates traffic: a note never turns a 200
    /// into a 503.
    pub fn notes(&self) -> Vec<&'static str> {
        let mut notes = Vec::new();
        if self.web_placeholder {
            notes.push("web: placeholder");
        }
        notes
    }
}

/// Serve `/metrics`, `/healthz` and `/readyz` on `addr` until the process ends.
///
/// `/healthz` is liveness: it answers 200 as long as the process can serve HTTP
/// at all, because restarting a UI that is merely waiting on the apiserver fixes
/// nothing. `/readyz` is the one that gates traffic.
///
/// Returning at all means the ops port is gone, which `main` treats as fatal —
/// see the `tokio::select!` there. A bind failure is by far the likeliest cause
/// and its error names [`crate::config::OPS_ADDR_ENV`], because the two things
/// that produce one are a port already in use and a `[::]` bind on a host with
/// IPv6 disabled, and both are fixed by setting that variable.
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
    // Plain text: this body is read by a human running `kubectl describe pod` or
    // `curl`, and it is the only place that says WHY. Notes ride along on both
    // answers — a placeholder web bundle is worth reporting and is never worth
    // refusing traffic over.
    let notes = state.readiness.notes();
    let suffix = if notes.is_empty() {
        String::new()
    } else {
        format!(" ({})", notes.join(", "))
    };

    let reasons = state.readiness.reasons();
    if reasons.is_empty() {
        return (StatusCode::OK, format!("ok{suffix}\n"));
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        format!("not ready: {}{suffix}\n", reasons.join(", ")),
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
        readiness.set_cache(CacheState::Synced);
        assert!(readiness.reasons().is_empty());
    }

    /// A deployment with the cache off has nothing to sync, so it must not be
    /// held un-ready waiting for it. This is the arm that a `cache_ready` flag
    /// nobody remembered to set would have got wrong forever.
    #[test]
    fn a_disabled_cache_never_holds_the_pod_un_ready() {
        let readiness = Readiness::new(false);
        readiness.impersonation_ok.store(true, Ordering::Relaxed);
        readiness.set_cache(CacheState::Disabled);
        assert!(readiness.reasons().is_empty());
    }

    /// Each way the cache can fail gets its own token, because the remediations
    /// differ: wait, check RBAC and the CRDs, or read the logs. A single
    /// `cache-not-ready` for all three would send every operator down the wrong
    /// path two times out of three.
    #[test]
    fn every_cache_state_reports_a_distinct_reason_and_survives_a_round_trip() {
        let cases = [
            (CacheState::Disabled, None),
            (CacheState::Syncing, Some("cache-not-ready")),
            (CacheState::Synced, None),
            (CacheState::SyncTimedOut, Some("cache-sync-timed-out")),
            (CacheState::WatchEnded, Some("cache-watch-ended")),
        ];

        let readiness = Readiness::new(false);
        for (state, reason) in cases {
            readiness.set_cache(state);
            assert_eq!(readiness.cache(), state, "{state:?} must round-trip");
            assert_eq!(state.blocking_reason(), reason, "{state:?}");
        }

        let tokens: std::collections::BTreeSet<_> = cases
            .iter()
            .filter_map(|(s, _)| s.blocking_reason())
            .collect();
        assert_eq!(
            tokens.len(),
            3,
            "two states sharing a token would send an operator down the wrong path",
        );
    }

    /// A bind failure must come back as an error naming the env var that fixes
    /// it, because `main` turns that error into the process's exit — a silently
    /// missing ops port would leave the pod unprobeable but apparently healthy.
    #[tokio::test]
    async fn a_bind_failure_names_the_env_var_that_fixes_it() {
        // Hold the port so `serve_ops` cannot have it. Port 0 lets the OS pick a
        // free one, so this never collides with a real service or another test.
        let held = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the test must be able to hold a port");
        let taken = held.local_addr().expect("a bound listener has an address");

        let metrics = Arc::new(UiMetrics::new(Arc::new(
            kopiur_telemetry::MetricsProvider::new("kopiur-ui-test"),
        )));
        let err = serve_ops(taken, metrics, Arc::new(Readiness::new(false)))
            .await
            .expect_err("binding an address another socket holds must fail");

        // `{:#}` renders the whole anyhow chain: the context plus the OS error.
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains(crate::config::OPS_ADDR_ENV),
            "the error must name the env var to change, got: {rendered}"
        );
        assert!(
            rendered.contains(&taken.to_string()),
            "the error must name the address it could not bind, got: {rendered}"
        );
    }

    /// A backend-only build serves a stand-in page. That is a valid development
    /// state — it is how `cargo run -p kopiur-ui` exercises the API half — so it
    /// is REPORTED but never refuses traffic. Making it a 503 would mean a
    /// checkout with no `web/dist` could not be reached at all, and a released
    /// image cannot hit the case anyway (`KOPIUR_UI_REQUIRE_WEB=1` turns a
    /// missing bundle into a build failure).
    #[tokio::test]
    async fn a_placeholder_web_bundle_is_reported_but_still_ready() {
        let readiness = Readiness::new(true);
        readiness.impersonation_ok.store(true, Ordering::Relaxed);
        readiness.set_cache(CacheState::Disabled);

        assert!(
            readiness.reasons().is_empty(),
            "a placeholder bundle must not gate traffic",
        );
        assert_eq!(readiness.notes(), vec!["web: placeholder"]);

        let (status, body) = readyz_body(readiness).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok (web: placeholder)\n");
    }

    /// The note rides along on a 503 too: an operator reading the probe output
    /// gets the whole picture, not just whichever half is currently louder.
    #[tokio::test]
    async fn a_not_ready_answer_still_carries_its_notes() {
        let readiness = Readiness::new(true);
        readiness.set_cache(CacheState::SyncTimedOut);

        let (status, body) = readyz_body(readiness).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            "not ready: impersonation-unavailable, cache-sync-timed-out (web: placeholder)\n"
        );
    }

    /// Run the real `/readyz` handler and return what a probe would see.
    async fn readyz_body(readiness: Readiness) -> (StatusCode, String) {
        let state = OpsState {
            metrics: Arc::new(UiMetrics::new(Arc::new(
                kopiur_telemetry::MetricsProvider::new("kopiur-ui-test"),
            ))),
            readiness: Arc::new(readiness),
        };
        let response = readyz(State(state)).await.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("readyz body");
        (status, String::from_utf8(bytes.to_vec()).expect("utf-8"))
    }
}
