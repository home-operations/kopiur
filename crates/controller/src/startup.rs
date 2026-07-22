//! Process startup: [`run`] — telemetry, the kube client, the HTTP server,
//! the leader-election gate, the shared Maintenance informer, self-managed
//! webhook TLS, and finally the controller fan-out
//! ([`crate::controllers::spawn_all`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;
use kube::runtime::events::{Recorder, Reporter};
use kube::runtime::watcher::Config as WatcherConfig;
use kube::runtime::{WatchStreamExt, reflector, watcher};
use kube::{Api, Client};

use kopiur_api::Maintenance;

use crate::config;
use crate::context::{Context, KopiaClientFactory};
use crate::controllers::{scoped_api, spawn_all};
use crate::http::serve_http;
use crate::leader;
use crate::metrics::Metrics;
use crate::sweep;
use crate::webhook_tls;

/// Resolve the effective streaming-list setting against the live apiserver.
///
/// kube's watcher does not degrade from WatchList to paged lists by itself, so we
/// only enable streaming when the server actually supports it. Returns `false`
/// immediately when streaming was not requested; otherwise probes the apiserver
/// version and downgrades (with a warning) on a server that predates WatchList. A
/// probe failure or unparseable version honors the configured value rather than
/// silently disabling an explicitly-requested optimization.
async fn effective_streaming_lists(client: &Client, configured: bool) -> bool {
    if !configured {
        return false;
    }
    let (major, minor) = match client.apiserver_version().await {
        Ok(info) => (info.major, info.minor),
        Err(e) => {
            tracing::warn!(error = %e, "apiserver version probe failed; honoring streamingLists as configured");
            return true;
        }
    };
    let supported = watchlist_supported(&major, &minor);
    if !supported {
        tracing::warn!(
            server_major = %major,
            server_minor = %minor,
            "streamingLists is on but the apiserver predates WatchList (beta in 1.32); \
             using paged lists instead. Set streamingLists: false to silence this."
        );
    }
    supported
}

/// Whether a Kubernetes apiserver of the given `major.minor` version ships the
/// WatchList feature (beta and on by default from 1.32). The version strings come
/// straight from `/version` and can carry suffixes (e.g. minor `"32+"` on some
/// distributions), so we read the leading digit run. An unparseable version
/// returns `true` (honor the configured request rather than second-guess it).
fn watchlist_supported(major: &str, minor: &str) -> bool {
    let leading_u32 = |s: &str| -> Option<u32> {
        let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    };
    match (leading_u32(major), leading_u32(minor)) {
        (Some(maj), Some(min)) => (maj, min) >= (1, 32),
        _ => true,
    }
}

/// Build the controller manager and run every controller concurrently, plus the
/// `/metrics` server, until shutdown.
///
/// Each `Controller` wires its owned-resource watches per ADR §5.2:
/// - `SnapshotSchedule` owns `Snapshot`.
/// - `SnapshotPolicy` watches `Snapshot` (GFS retention).
/// - `Repository`/`ClusterRepository` watch discovered `Snapshot`.
/// - `Snapshot` owns `Job` + `ConfigMap` (mover run).
/// - `Restore` watches the target `PVC` (populator handshake).
pub async fn run(config: config::ControllerConfig) -> anyhow::Result<()> {
    // Install the tracing subscriber (fmt + OTLP traces/logs when configured).
    // Held for the process lifetime so buffered OTLP spans/logs flush on exit.
    // Errors only surface under KOPIUR_OTEL_STRICT; otherwise OTLP degrades to
    // fmt-only and the call succeeds.
    let _telemetry = kopiur_telemetry::init_tracing("kopiur-controller")?;

    // Install the process-level rustls CryptoProvider before the kube client
    // builds any TLS config; without this, kube's rustls-tls backend panics with
    // "no process-level CryptoProvider available". Idempotent: ignore the error
    // if a provider is already installed (e.g. the webhook installed it).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // One inferred config, two clients (`Client::try_default` is exactly
    // infer + try_from, so behavior is otherwise identical):
    // - `client`: read/connect timeouts hardened (see the config consts — the
    //   apiserver-outage fd fix). Everything rides this one.
    // - `exec_client`: same connect timeout but NO read timeout, used ONLY for
    //   the hooks `workloadExec` attach — the exec WebSocket rides the same
    //   timeout-wrapped connector as unary calls, and a read timeout would
    //   kill any quiesce command that stays silent longer than the window.
    let mut kube_config = kube::Config::infer().await?;
    kube_config.connect_timeout = Some(config::KUBE_CLIENT_CONNECT_TIMEOUT);
    kube_config.read_timeout = Some(config::KUBE_CLIENT_READ_TIMEOUT);
    let client = Client::try_from(kube_config.clone())?;
    let mut exec_config = kube_config;
    exec_config.read_timeout = None;
    let exec_client = Client::try_from(exec_config)?;
    let metrics = Metrics::new();

    // The HTTP server (probes + /metrics) starts BEFORE the leader-election
    // gate: a standby replica must pass its liveness/readiness probes while it
    // waits for the Lease, or the kubelet would kill the very replica that is
    // supposed to take over on failover.
    let http_srv = tokio::spawn(serve_http(metrics.clone(), config.http_addr));

    // Leader election (--leader-elect): block until this replica holds the
    // Lease, then keep renewing it. Everything below this point — reconcilers,
    // the shared informer, self-managed webhook TLS — is leader-only work.
    // `leadership_lost` completes only if the Lease is lost; that is fatal
    // (exit, restart, re-elect) — see [`leader`] for why. A missing-RBAC 403
    // degrades to no-election (`Acquired::Degraded`, loudly) so a new image
    // under an old chart — which stamps --leader-elect=true but grants no
    // leases RBAC — keeps running instead of crash-looping.
    let leadership = match &config.leader_election {
        Some(le) => {
            let identity = leader_identity();
            match leader::acquire(&client, le, &identity).await {
                leader::Acquired::Leading => Some((le.clone(), identity)),
                leader::Acquired::Degraded => None,
            }
        }
        None => None,
    };
    let leadership_lost = leadership
        .as_ref()
        .map(|(le, id)| leader::spawn_renewal(client.clone(), le.clone(), id.clone()));

    let reporter = Reporter::from("kopiur-controller");
    let recorder = Recorder::new(client.clone(), reporter);
    // Everything below reads the pre-resolved config (flags with KOPIUR_* env
    // fallback, parsed in main): defaults applied, empty strings filtered,
    // closed value sets already typed. The startup log lines keep the same
    // shape so operators grepping for them see no change.
    tracing::info!(watch_scope = ?config.watch_scope, "install scope configured");
    tracing::info!(mover_image = %config.mover_image, "mover image configured");
    tracing::info!(mover_service_account = ?config.mover_service_account, "mover SA configured");
    tracing::info!(
        mover_clusterrole = %config.mover_clusterrole,
        mover_role_kind = %config.mover_role_kind.as_str(),
        "mover role configured"
    );
    tracing::info!(operator_namespace = ?config.operator_namespace, "operator namespace configured");
    // Telemetry + logging env the controller passes through to mover Jobs: OTLP
    // (when a collector is configured) plus RUST_LOG / KOPIUR_LOG_FORMAT so movers
    // inherit the controller's log level and format. (Still read from the process
    // env: these names are owned by the telemetry crate and forwarded verbatim.)
    let mover_env_passthrough = collect_mover_env_passthrough();

    // The writable base for the controller's in-process kopia cache/logs/config
    // (an emptyDir the chart mounts at the default). Overridable only if that
    // mount is relocated; without it kopia would try $HOME (/nonexistent) on the
    // read-only rootfs and fail to create its cache.
    let kopia_factory = match &config.kopia_cache_dir {
        Some(dir) => {
            tracing::info!(kopia_cache_dir = %dir, "kopia cache dir overridden");
            KopiaClientFactory::new().with_cache_dir(dir)
        }
        None => KopiaClientFactory::new(),
    };

    // Shared Maintenance informer: a single reflector-backed cache the
    // Repository/ClusterRepository reconcilers read to answer "is a Maintenance
    // configured for me?" without an `Api::list` per reconcile. We drive the
    // reflector stream ourselves in a spawned task (a standalone `Store`'s
    // `wait_until_ready()` does NOT drive the underlying watch — kube requires the
    // reflector stream to be polled separately), and flip `maintenance_synced`
    // once the initial list completes so a cold cache never yields a false
    // "not configured" warning on startup.
    let (maintenance_store, maintenance_writer) = reflector::store::<Maintenance>();
    let maintenance_synced = Arc::new(AtomicBool::new(false));
    {
        let reader = maintenance_store.clone();
        let synced = maintenance_synced.clone();
        let api: Api<Maintenance> = scoped_api(&client, &config.watch_scope);
        tokio::spawn(async move {
            // Flip the flag as soon as the reflector reports its first sync.
            let mark_ready = async move {
                if reader.wait_until_ready().await.is_ok() {
                    synced.store(true, Ordering::Relaxed);
                    tracing::info!("maintenance informer cache synced");
                } else {
                    tracing::warn!("maintenance informer writer dropped before sync");
                }
            };
            // Drive the watch → reflector store forever (with backoff on errors).
            let drive = async move {
                let stream = reflector(maintenance_writer, watcher(api, WatcherConfig::default()))
                    .default_backoff()
                    .touched_objects();
                futures::pin_mut!(stream);
                while let Some(ev) = stream.next().await {
                    if let Err(e) = ev {
                        tracing::debug!(error = %e, "maintenance informer watch error");
                    }
                }
            };
            tokio::join!(mark_ready, drive);
        });
    }

    let ctx = Arc::new(Context::new(
        client.clone(),
        exec_client,
        kopia_factory,
        metrics.clone(),
        recorder,
        config.mover_image.clone(),
        config.mover_image_overridden,
        config.mover_pull_policy,
        config.mover_service_account.clone(),
        config.mover_clusterrole.clone(),
        config.mover_role_kind,
        mover_env_passthrough,
        maintenance_store,
        maintenance_synced,
        config.operator_namespace.clone(),
        config.watch_scope.clone(),
        config.max_concurrent_delete_jobs,
    ));

    // Self-managed webhook TLS (`webhook.tls.mode: self`): mint the serving cert
    // and inject the caBundle so the API server trusts the webhook — no
    // cert-manager. Best-effort at boot (the webhook configs may not exist yet on
    // a first apply); a slow background task then handles drift + leaf rotation.
    // Absent the managed-mode config, this is a no-op (cert-manager / manual mode).
    if let Some(webhook_tls) = webhook_tls_config(&config) {
        let ns = webhook_tls.namespace.clone();
        let boot_ok = match webhook_tls::ensure(&client, &webhook_tls).await {
            Ok(()) => {
                tracing::info!(namespace = %ns, "self-managed webhook TLS ready");
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "initial webhook TLS setup failed; retrying shortly");
                false
            }
        };
        spawn_webhook_tls_reconcile(client.clone(), webhook_tls, boot_ok);
    }

    tracing::info!("starting kopiur controllers");

    // Gate WatchList streaming lists on the apiserver version. kube's watcher does
    // NOT fall back from WatchList to paged lists on its own — if streaming is on
    // and the server lacks the feature, the initial `sendInitialEvents` watch fails
    // and the watcher retries the streaming path forever instead of degrading. So
    // resolve it here: on a server that predates WatchList (beta, on by default
    // from 1.32) we force paged lists regardless of config.
    let streaming_lists = effective_streaming_lists(&client, config.streaming_lists).await;

    // Backstop GC for mover work-spec ConfigMaps whose Job is already gone
    // (TTL-reaped before the reconciler observed it, or left behind by operator
    // versions that never deleted them). Leader-only, like everything below the
    // election gate — a single sweeping writer.
    sweep::spawn_sweep(
        client.clone(),
        config.watch_scope.clone(),
        metrics.clone(),
        config.work_spec_sweep_interval_secs,
        config.work_spec_sweep_min_age_secs,
    );

    let controllers = spawn_all(
        client.clone(),
        ctx,
        streaming_lists,
        config.watch_scope.clone(),
        config.reconcile_concurrency,
    );
    tracing::info!(
        reconcile_concurrency = ?config.reconcile_concurrency.map(std::num::NonZeroU16::get),
        "per-controller reconcile concurrency configured (None = unbounded)"
    );

    match leadership_lost {
        Some(lost) => {
            tokio::select! {
                _ = controllers => tracing::warn!("all controllers exited"),
                r = http_srv => tracing::warn!(?r, "http server exited"),
                _ = lost => {
                    // Continuing to reconcile without the Lease would be a
                    // split-brain double-reconcile; exit non-zero so the pod
                    // restarts and re-enters the election.
                    anyhow::bail!("leader lease lost; exiting to re-elect");
                }
                _ = shutdown_signal() => {
                    // Graceful shutdown while leading: release the Lease so
                    // the successor claims it immediately, instead of every
                    // rolling upgrade stalling reconciliation for the full
                    // lease duration while the dead holder's Lease ages out.
                    tracing::info!("shutdown signal received; releasing leader lease");
                    if let Some((le, id)) = &leadership {
                        leader::release(&client, le, id).await;
                    }
                }
            }
        }
        None => {
            tokio::select! {
                _ = controllers => tracing::warn!("all controllers exited"),
                r = http_srv => tracing::warn!(?r, "http server exited"),
                _ = shutdown_signal() => tracing::info!("shutdown signal received"),
            }
        }
    }
    Ok(())
}

/// Resolve on SIGTERM (Kubernetes pod termination) or Ctrl-C, so `run` gets a
/// graceful-shutdown branch (used to release the leader Lease on the way out).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
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

/// This replica's leader-election identity: the pod name (`HOSTNAME`, set by
/// the kubelet), falling back to a pid-suffixed name for off-cluster runs so
/// two local processes never share an identity.
fn leader_identity() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| format!("kopiur-controller-{}", std::process::id()))
}

/// Collect the env vars the controller stamps onto every mover `Job` so a mover
/// inherits the controller's telemetry + logging configuration. Two groups:
///
/// - **OTLP** (`OTEL_EXPORTER_OTLP_*`): forwarded only when a collector endpoint
///   is set, so movers stay fully offline (fmt-only) otherwise.
/// - **Logging** (`RUST_LOG`, `KOPIUR_LOG_FORMAT`): forwarded whenever present,
///   regardless of OTLP, so `kubectl logs` on a mover Job honors the same level
///   and format the controller runs with.
///
/// `(name, value)` pairs, de-duplicated by name (the two groups don't overlap).
fn collect_mover_env_passthrough() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();

    // OTLP only when a collector is configured.
    if std::env::var(config::OTEL_EXPORTER_OTLP_ENDPOINT).is_ok() {
        env.extend(
            config::OTLP_PASSTHROUGH
                .iter()
                .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v))),
        );
    }

    // Logging always (when set in the controller's env).
    env.extend(
        config::LOG_PASSTHROUGH
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v))),
    );

    env
}

/// Assemble the [`webhook_tls::WebhookTlsConfig`] from the resolved controller
/// config, or `None` when the chart did not enable self-managed webhook TLS
/// (cert-manager / manual mode, or off-chart). Requires the managed gate plus a
/// known operator namespace and both webhook-configuration names; a partial
/// config is treated as not-managed and logged, rather than guessed. Pure over
/// its input, so the skip decisions are unit-testable.
fn webhook_tls_config(config: &config::ControllerConfig) -> Option<webhook_tls::WebhookTlsConfig> {
    if !config.webhook_tls_managed {
        return None;
    }

    let namespace = match &config.operator_namespace {
        Some(ns) => ns.clone(),
        None => {
            tracing::warn!(
                "{} is set but {} is unset; cannot place the webhook TLS Secret — skipping \
                 self-managed webhook TLS",
                config::WEBHOOK_TLS_MANAGED_ENV,
                config::OPERATOR_NAMESPACE_ENV
            );
            return None;
        }
    };
    let (Some(validating_config), Some(mutating_config)) = (
        config.webhook_validating_config.clone(),
        config.webhook_mutating_config.clone(),
    ) else {
        tracing::warn!(
            "self-managed webhook TLS requested but {}/{} are unset — skipping",
            config::WEBHOOK_VALIDATING_CONFIG_ENV,
            config::WEBHOOK_MUTATING_CONFIG_ENV
        );
        return None;
    };

    Some(webhook_tls::WebhookTlsConfig {
        namespace,
        secret_name: config.webhook_secret_name.clone(),
        service_name: config.webhook_service_name.clone(),
        validating_config,
        mutating_config,
    })
}

/// Drive [`webhook_tls::ensure`] in the background so the serving leaf rotates
/// before expiry and the `caBundle` self-heals if anything overwrites it.
///
/// The cadence is adaptive: after a success it waits the slow steady-state
/// interval ([`config::WEBHOOK_TLS_RECONCILE_INTERVAL`]); after a failure it
/// retries soon ([`config::WEBHOOK_TLS_RETRY_INTERVAL`]). This matters at boot —
/// if the webhook configurations aren't registered yet when the controller
/// starts, the first inject fails, and a fixed slow tick would leave admission
/// untrusted for hours. `boot_ok` seeds the first wait from the boot attempt's
/// result. Errors are logged, never fatal (degrade-not-crash).
fn spawn_webhook_tls_reconcile(client: Client, cfg: webhook_tls::WebhookTlsConfig, boot_ok: bool) {
    tokio::spawn(async move {
        let mut ok = boot_ok;
        loop {
            let delay = if ok {
                config::WEBHOOK_TLS_RECONCILE_INTERVAL
            } else {
                config::WEBHOOK_TLS_RETRY_INTERVAL
            };
            tokio::time::sleep(delay).await;
            ok = match webhook_tls::ensure(&client, &cfg).await {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(error = %e, "webhook TLS reconcile failed; will retry soon");
                    false
                }
            };
        }
    });
}

#[cfg(test)]
mod tests {
    use super::watchlist_supported;

    #[test]
    fn watchlist_supported_gates_on_1_32() {
        // At or above 1.32 → supported.
        assert!(watchlist_supported("1", "32"));
        assert!(watchlist_supported("1", "34"));
        assert!(watchlist_supported("2", "0"));
        // Distribution version suffixes (EKS/GKE style) read the leading digits.
        assert!(watchlist_supported("1", "32+"));
        assert!(watchlist_supported("1", "33-eks-1234"));
        // Below 1.32 → not supported (force paged lists).
        assert!(!watchlist_supported("1", "31"));
        assert!(!watchlist_supported("1", "24"));
        assert!(!watchlist_supported("1", "31+"));
    }

    #[test]
    fn watchlist_supported_honors_config_on_unparseable_version() {
        // An unreadable version string must not silently disable an explicit
        // streamingLists request — honor it (return true).
        assert!(watchlist_supported("", ""));
        assert!(watchlist_supported("x", "y"));
        assert!(watchlist_supported("1", ""));
    }
}
