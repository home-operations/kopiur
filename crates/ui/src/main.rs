//! `kopiur-ui` — the web UI backend's entry point.
//!
//! Startup order is deliberate and load-bearing:
//!
//! 1. **Parse and resolve the configuration.** A misconfiguration that would make
//!    identity ambiguous must kill the process with an actionable message, before
//!    any listener exists — see [`kopiur_ui::config`]. Exit code 2 (not 1) so a
//!    supervisor can tell "you configured this wrong" from "it crashed".
//! 2. **Tracing**, held for the process lifetime so buffered OTLP data flushes.
//! 3. **The rustls crypto provider**, before anything can build a TLS config.
//! 4. **The metrics provider**, so nothing records into a provider that does not
//!    exist yet.
//! 5. **The ops listener** — `/metrics`, `/healthz`, `/readyz` — FIRST, before the
//!    kube client or the app server. A UI that is failing to start is then still
//!    observable: the kubelet gets `/readyz` telling it what is not ready, rather
//!    than a connection refused it can only report as CrashLoopBackOff.
//! 6. **The kube client, the auth state, the read source and the session pool.**
//!    A failure to build the client is fatal: with no apiserver the UI can do
//!    nothing at all, and the message names what to fix.
//! 7. **The impersonation self-check and the cache watchers**, both in the
//!    background. Neither blocks the app server from binding — the pod stays
//!    unready and `/readyz` says which one is outstanding — because a UI that
//!    cannot be reached at all is a UI nobody can diagnose.
//! 8. **The app server**, with graceful shutdown on SIGTERM.
//!
//! The two servers are then raced with `tokio::select!`, so whichever fails
//! first ends the process carrying its own error. The ops listener failing to
//! bind is fatal for the same reason it starts first: probes are how the
//! Deployment learns this pod is broken, and a process serving the app behind a
//! dead ops port is both unprobeable and unmonitorable while looking healthy.
//!
//! Configuration is flag > env > default; every env name lives in
//! [`kopiur_ui::config`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clap::Parser as _;
use tokio::signal;

use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
};
use kube::api::{Api, PostParams};

use kopiur_ui::config::{UiArgs, UiConfig};
use kopiur_ui::metrics::UiMetrics;
use kopiur_ui::ops_listener::{CacheState, Readiness, serve_ops};
use kopiur_ui::startup::{
    CORE_GROUP, ImpersonationTarget, impersonation_targets, track_cache_readiness,
};
use kopiur_ui::{AppState, app, auth, browse, cache, static_files};

/// Exit code for a refused configuration, distinct from a crash.
const EXIT_BAD_CONFIG: i32 = 2;

/// How often the serving certificate is re-read from disk, so a rotated leaf is
/// picked up without a pod restart.
const TLS_RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // clap first: a bad flag or env value must fail with clap's own actionable
    // message before any subsystem starts.
    let args = UiArgs::parse();

    // Resolve before tracing so the message is unmissable on stderr rather than
    // buried in a structured log line — this is the error an operator will hit
    // most often, and it is always something they can fix in their values file.
    let cfg = match args.resolve() {
        Ok(cfg) => Arc::new(cfg),
        Err(e) => {
            eprintln!("kopiur-ui: refusing to start: {e}");
            std::process::exit(EXIT_BAD_CONFIG);
        }
    };

    let _telemetry = kopiur_telemetry::init_tracing("kopiur-ui")?;

    // Required before building any rustls ServerConfig. Ignore the error if a
    // provider is already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing::info!(config = ?cfg, "resolved kopiur-ui configuration");

    let provider = Arc::new(kopiur_telemetry::MetricsProvider::new("kopiur-ui"));
    let metrics = Arc::new(UiMetrics::new(provider));

    let readiness = Arc::new(Readiness::new(static_files::is_placeholder()));
    if readiness.web_placeholder {
        tracing::warn!(
            "this build embeds the placeholder web page, not the SPA: /readyz will report \
             placeholder-web. Run `mise run ui-build` and rebuild, or use an image built with \
             KOPIUR_UI_REQUIRE_WEB=1."
        );
    }

    // SPAWNED, not merely awaited later: the listener has to be binding and
    // answering while the rest of startup (Task 8's kube client and reflector
    // stores) is still running, which is the whole point of starting it first.
    let ops_addr = cfg.ops_addr;
    let ops = tokio::spawn({
        let metrics = Arc::clone(&metrics);
        let readiness = Arc::clone(&readiness);
        async move { serve_ops(ops_addr, metrics, readiness).await }
    });

    // One inferred config, two uses: the UI's OWN ServiceAccount client (the
    // impersonation self-check and, when enabled, the reflector watches) and the
    // base every per-caller impersonating client is cloned from. `AuthState::new`
    // sanitizes the base itself — any impersonation the configuration carried
    // would otherwise be *appended* to the identity each client asserts.
    //
    // Fatal, unlike everything below it: with no apiserver there is nothing the
    // UI can serve, and no amount of waiting fixes a missing kubeconfig.
    let base_config = kube::Config::infer().await.map_err(|e| {
        anyhow::Error::new(e).context(
            "kopiur-ui could not work out how to reach the apiserver. In a cluster this \
             means the pod has no ServiceAccount token mounted (check \
             automountServiceAccountToken and the chart's ui.serviceAccount); outside one \
             it means there is no usable kubeconfig at $KUBECONFIG or ~/.kube/config.",
        )
    })?;
    let ui_client = kube::Client::try_from(base_config.clone()).map_err(|e| {
        anyhow::Error::new(e).context(
            "kopiur-ui could not build a kube client from the inferred configuration; the \
             cluster URL or its CA bundle is unusable",
        )
    })?;

    let auth = Arc::new(auth::AuthState::new(
        cfg.auth.clone(),
        base_config,
        cfg.client_cache.clone(),
    ));

    // Readiness is answered in the background, both halves. Binding the app port
    // is not gated on either: a pod that cannot be reached is a pod nobody can
    // diagnose, and `/readyz` keeps it out of the Service until both are up.
    tokio::spawn(check_impersonation(
        ui_client.clone(),
        Arc::clone(&cfg),
        Arc::clone(&readiness),
    ));

    let source = build_source(&cfg, &ui_client, &metrics, &readiness);

    let state = AppState {
        cfg: Arc::clone(&cfg),
        metrics,
        readiness,
        auth,
        source: Arc::new(source),
        sessions: Arc::new(browse::session_pool::SessionPool::new(cfg.session.clone())),
    };

    let router = app(state);
    let addr = cfg.addr;

    let serve_app = async {
        match &cfg.tls {
            Some(tls) => {
                let (cert, key) = (tls.cert.clone(), tls.key.clone());
                tracing::info!(%addr, cert = %cert.display(), key = %key.display(),
                    "serving the kopiur-ui app over HTTPS");
                serve_tls(addr, router, &cert, &key).await
            }
            None => {
                tracing::info!(
                    %addr,
                    "serving the kopiur-ui app over plain HTTP; TLS is expected to terminate \
                     at the authenticating proxy in front of it (set \
                     KOPIUR_UI_TLS_CERT/KEY to terminate here instead)"
                );
                serve_http(addr, router).await
            }
        }
    };

    // Whichever server ends first ends the process, carrying its own error.
    //
    // The ops listener failing is FATAL, not a degraded mode. `/readyz` is how
    // the Deployment learns this pod cannot serve, and `/metrics` is how anyone
    // learns anything else: a process that kept serving the app with a dead ops
    // port would look healthy to Kubernetes forever while being unmonitorable,
    // and would never be rolled back. A non-zero exit turns that into a
    // CrashLoopBackOff with the bind error in the pod's logs — which is the
    // outcome an operator can actually act on.
    //
    // `select!` also drops the loser: the app server shutting down gracefully on
    // SIGTERM tears the ops listener down with it, so nothing outlives the
    // process's own shutdown.
    tokio::select! {
        served = serve_app => served,
        joined = ops => Err(ops_ended_early(joined)),
    }
}

/// Build the read source, and everything that keeps its readiness honest.
///
/// Exhaustive on the one configuration bit that decides it, and it decides two
/// things at once — where reads come from, and what `/readyz` is waiting for —
/// which is why they are set together here rather than in two places that could
/// disagree. With the cache off there is nothing to sync, so the cache half of
/// readiness is [`CacheState::Disabled`] immediately; a flag nobody set would
/// otherwise hold the pod unready forever.
///
/// The reflector tasks are owned by the supervisor `cache::stores::start`
/// spawns, and [`track_cache_readiness`] follows the health it publishes for the
/// process's life — including a reflector dying long after the initial sync,
/// which must take `/readyz` down with it.
fn build_source(
    cfg: &UiConfig,
    ui_client: &kube::Client,
    metrics: &Arc<UiMetrics>,
    readiness: &Arc<Readiness>,
) -> cache::Source {
    if !cfg.cache_enabled {
        tracing::info!(
            "the read cache is off (KOPIUR_UI_CACHE=false): every read is one impersonated \
             call, authorized by the apiserver on the real request"
        );
        readiness.set_cache(CacheState::Disabled);
        return cache::Source::Impersonated;
    }

    tracing::info!(
        "starting the read cache: nine reflector stores under the UI's own ServiceAccount, \
         with every read gated by a SubjectAccessReview for the caller"
    );
    let (stores, health_rx) = cache::stores::start(ui_client.clone(), Arc::clone(metrics));
    tokio::spawn(track_cache_readiness(health_rx, Arc::clone(readiness)));

    cache::Source::Cache {
        stores,
        sar: cache::authz::SarCache::new(
            ui_client.clone(),
            cfg.sar_ttl,
            cfg.sar_cache_size,
            Arc::clone(metrics),
        ),
    }
}

/// Ask the apiserver, as the UI's own ServiceAccount, whether it may impersonate.
///
/// This is the one permission the whole design rests on. Without it every
/// request fails with a 403 the caller cannot fix and cannot understand, so the
/// pod must not take traffic — and the message has to name the RBAC, because
/// "impersonate" is exactly the permission a security-conscious operator trims.
///
/// A `SelfSubjectAccessReview` rather than a trial impersonation: it asks the
/// authorizer directly, needs no victim user to impersonate, and is answered the
/// same way by every authorization mode.
async fn check_impersonation(client: kube::Client, cfg: Arc<UiConfig>, readiness: Arc<Readiness>) {
    let denied = denied_impersonations(&client, &cfg).await;
    readiness
        .impersonation_ok
        .store(denied.is_empty(), Ordering::Relaxed);

    if denied.is_empty() {
        tracing::info!(
            "kopiur-ui may impersonate; every apiserver call will be made as the caller"
        );
        return;
    }
    tracing::error!(
        denied = ?denied,
        "kopiur-ui's ServiceAccount may not impersonate, so every request would be refused \
         with a 403 the caller cannot fix. Grant it `impersonate` on these resources in the \
         core API group (the chart's ui.rbac does this); until then /readyz reports \
         impersonation-unavailable and this pod stays out of the Service."
    );
}

/// Which of the resources this deployment must impersonate it may not.
///
/// A review that could not be *asked* counts as denied. Treating an unanswerable
/// question as permission is how a fail-open ships: the pod would join the
/// Service and 403 every caller.
async fn denied_impersonations(client: &kube::Client, cfg: &UiConfig) -> Vec<String> {
    let mut denied = Vec::new();
    for target in impersonation_targets(cfg) {
        let allowed = match may_impersonate(client, &target).await {
            Ok(allowed) => allowed,
            Err(e) => {
                tracing::error!(
                    target = %target,
                    error = %e,
                    "kopiur-ui could not ask the apiserver whether it may impersonate; \
                     treating it as denied. /readyz reports impersonation-unavailable."
                );
                false
            }
        };
        if !allowed {
            denied.push(target.to_string());
        }
    }
    denied
}

/// One `SelfSubjectAccessReview` for `impersonate` on one target.
///
/// The target's name is sent when it has one. Omitting it is not a harmless
/// simplification: RBAC answers the unnamed question only from rules that carry
/// no `resourceNames`, so against the narrow rule the chart ships for anonymous
/// mode an unnamed review is correctly DENIED while every real request — which
/// does name a principal — would have been allowed.
async fn may_impersonate(
    client: &kube::Client,
    target: &ImpersonationTarget,
) -> Result<bool, kube::Error> {
    let (resource, subresource) = match target.resource.split_once('/') {
        Some((head, tail)) => (head.to_string(), Some(tail.to_string())),
        None => (target.resource.clone(), None),
    };
    let review = SelfSubjectAccessReview {
        metadata: Default::default(),
        spec: SelfSubjectAccessReviewSpec {
            resource_attributes: Some(ResourceAttributes {
                verb: Some("impersonate".to_string()),
                // From the target, not a constant: `userextras/<key>` lives in
                // authentication.k8s.io while `users` and `groups` live in the
                // core group, and asking in the wrong one is denied exactly like
                // asking with the wrong name.
                group: Some(target.api_group.clone()),
                resource: Some(resource),
                subresource,
                name: target.name.clone(),
                ..Default::default()
            }),
            ..Default::default()
        },
        status: None,
    };
    let api: Api<SelfSubjectAccessReview> = Api::all(client.clone());
    let reviewed = api.create(&PostParams::default(), &review).await?;
    Ok(reviewed.status.is_some_and(|status| status.allowed))
}

/// Turn "the ops server finished before the app server did" into the error that
/// ends the process.
///
/// Every outcome is a failure here, including `Ok(())`: [`serve_ops`] runs until
/// the process does, so its returning at all means the ops port is gone.
fn ops_ended_early(joined: Result<anyhow::Result<()>, tokio::task::JoinError>) -> anyhow::Error {
    const CONSEQUENCE: &str = "without it Kubernetes cannot probe this pod and nothing can \
                               scrape its metrics, so the process exits rather than keep \
                               serving unmonitored";
    match joined {
        // The usual case: the bind failed, and this error carries the address
        // and the KOPIUR_UI_OPS_ADDR remediation from `serve_ops`.
        Ok(Err(e)) => e.context(format!(
            "the kopiur-ui ops server (/metrics, /healthz, /readyz) failed; {CONSEQUENCE}"
        )),
        Ok(Ok(())) => anyhow::anyhow!(
            "the kopiur-ui ops server (/metrics, /healthz, /readyz) stopped on its own while \
             the app server was still running; {CONSEQUENCE}"
        ),
        Err(e) => anyhow::Error::new(e).context(format!(
            "the kopiur-ui ops server (/metrics, /healthz, /readyz) task panicked; {CONSEQUENCE}"
        )),
    }
}

/// Serve `router` on `addr` over plain HTTP until SIGTERM or Ctrl-C.
///
/// The default: TLS normally terminates at the authenticating proxy in front of
/// the UI, so the app port carries plain HTTP inside the cluster.
async fn serve_http(addr: SocketAddr, router: axum::Router) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
        format!(
            "binding the kopiur-ui app server to {addr}; if this host has IPv6 disabled a \
             `[::]` bind fails — set KOPIUR_UI_ADDR=0.0.0.0:{} (via the chart's ui.extraEnv)",
            addr.port()
        )
    })?;
    tracing::info!(%addr, "listening (http)");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Serve `router` on `addr` over HTTPS until SIGTERM or Ctrl-C, terminating TLS
/// with the cert/key at `cert_path`/`key_path`.
///
/// Used only when `KOPIUR_UI_TLS_CERT`/`_KEY` are both set — for a deployment
/// that reaches the UI without a TLS-terminating proxy in front of it. The
/// serving cert is re-read periodically ([`spawn_cert_reload`]) so a rotation
/// does not need a pod restart.
async fn serve_tls(
    addr: SocketAddr,
    router: axum::Router,
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use axum_server::Handle;
    use axum_server::tls_rustls::RustlsConfig;

    let config = RustlsConfig::from_pem_file(cert_path, key_path)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to load the kopiur-ui TLS cert/key from {} and {}: {e}",
                cert_path.display(),
                key_path.display()
            )
        })?;

    // Hot-reload the serving cert so a rotated leaf (cert-manager rewrites the
    // Secret, the kubelet syncs it into these files) is served without a pod
    // restart. A reload failure — mid-write files, most often — keeps the current
    // config and is retried next tick; it is never fatal.
    spawn_cert_reload(
        config.clone(),
        cert_path.to_path_buf(),
        key_path.to_path_buf(),
    );

    let handle = Handle::new();
    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received; draining connections");
        shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
    });

    tracing::info!(%addr, "listening (https)");
    axum_server::bind_rustls(addr, config)
        .handle(handle)
        .serve(router.into_make_service())
        .await
        .with_context(|| {
            format!(
                "binding the kopiur-ui TLS server to {addr}; if this host has IPv6 disabled a \
                 `[::]` bind fails — set KOPIUR_UI_ADDR=0.0.0.0:{} (via the chart's \
                 ui.extraEnv)",
                addr.port()
            )
        })?;
    Ok(())
}

/// Periodically re-read the cert/key PEM files into the live `RustlsConfig`.
/// The first tick fires immediately and is skipped — the config was just loaded.
fn spawn_cert_reload(
    config: axum_server::tls_rustls::RustlsConfig,
    cert_path: std::path::PathBuf,
    key_path: std::path::PathBuf,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TLS_RELOAD_INTERVAL);
        ticker.tick().await; // consume the immediate first tick
        loop {
            ticker.tick().await;
            match config.reload_from_pem_file(&cert_path, &key_path).await {
                Ok(()) => tracing::debug!("reloaded the kopiur-ui TLS cert from disk"),
                Err(e) => tracing::warn!(
                    error = %e,
                    "failed to reload the kopiur-ui TLS cert; keeping the current one"
                ),
            }
        }
    });
}

/// Resolve on SIGTERM (Kubernetes pod termination) or Ctrl-C.
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
