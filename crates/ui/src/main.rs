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
//! 6. **The app server**, with graceful shutdown on SIGTERM.
//!
//! Configuration is flag > env > default; every env name lives in
//! [`kopiur_ui::config`].

use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser as _;
use tokio::signal;

use kopiur_ui::config::UiArgs;
use kopiur_ui::metrics::UiMetrics;
use kopiur_ui::ops_listener::{Readiness, serve_ops};
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

    // Started first so the process is observable while the rest comes up.
    let ops_addr = cfg.ops_addr;
    let ops = tokio::spawn({
        let metrics = Arc::clone(&metrics);
        let readiness = Arc::clone(&readiness);
        async move { serve_ops(ops_addr, metrics, readiness).await }
    });

    // Task 8 builds the kube client, the reflector stores and the impersonating
    // client cache here, and flips the readiness flags as each comes up.
    let state = AppState {
        cfg: Arc::clone(&cfg),
        metrics,
        readiness,
        auth: Arc::new(auth::AuthState::default()),
        source: Arc::new(cache::Source::Impersonated),
        sessions: Arc::new(browse::session_pool::SessionPool::default()),
    };

    let router = app(state);
    let addr = cfg.addr;

    let served = match &cfg.tls {
        Some(tls) => {
            let (cert, key) = (tls.cert.clone(), tls.key.clone());
            tracing::info!(%addr, cert = %cert.display(), key = %key.display(),
                "serving the kopiur-ui app over HTTPS");
            serve_tls(addr, router, &cert, &key).await
        }
        None => {
            tracing::info!(
                %addr,
                "serving the kopiur-ui app over plain HTTP; TLS is expected to terminate at \
                 the authenticating proxy in front of it (set KOPIUR_UI_TLS_CERT/KEY to \
                 terminate here instead)"
            );
            serve_http(addr, router).await
        }
    };

    // The ops listener outlives the app server only long enough to report the
    // shutdown; aborting it keeps a bind failure there from hanging the process.
    ops.abort();
    served
}

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
