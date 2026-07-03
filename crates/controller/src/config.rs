//! The single place for the controller's runtime configuration: the names of
//! every environment variable it reads, plus fixed config values (bind
//! addresses). Domain string constants (labels/finalizers/annotations) live in
//! [`crate::consts`]; OTLP env var names are owned by [`kopiur_telemetry::env`]
//! and re-exported here so callers have one import.

/// Container image the controller stamps into every mover `Job`. Overrides
/// [`crate::jobs::DEFAULT_MOVER_IMAGE`] when set.
pub const MOVER_IMAGE_ENV: &str = "KOPIUR_MOVER_IMAGE";

/// ServiceAccount the mover `Job` pods run as. A dedicated least-privilege SA
/// (NOT the operator SA): the controller mints it — plus a `RoleBinding` to the
/// mover role named by [`MOVER_CLUSTERROLE_ENV`] — in each mover Job's namespace,
/// because a mover Job runs in the workload namespace where the operator SA does
/// not exist (ADR §4.12).
pub const MOVER_SERVICE_ACCOUNT_ENV: &str = "KOPIUR_MOVER_SERVICE_ACCOUNT";

/// Name of the mover `ClusterRole` (cluster install) / `Role` (namespaced install)
/// shipped by the chart, that the controller-minted per-namespace mover
/// `RoleBinding` references. Defaults to [`DEFAULT_MOVER_NAME`].
pub const MOVER_CLUSTERROLE_ENV: &str = "KOPIUR_MOVER_CLUSTERROLE";

/// `roleRef.kind` for the minted mover `RoleBinding`: `ClusterRole` for a
/// cluster-scoped install (one shared mover ClusterRole, bound per namespace) or
/// `Role` for a namespaced install (a mover Role in the operator's namespace). The
/// chart sets this from `installScope`; defaults to [`DEFAULT_MOVER_ROLE_KIND`].
pub const MOVER_ROLE_KIND_ENV: &str = "KOPIUR_MOVER_ROLE_KIND";

/// Fallback name for the mover ServiceAccount and mover Role/ClusterRole when the
/// respective env var is unset (off-chart / test runs). Matches the chart's
/// `kopiur.moverName` helper for the default release name.
pub const DEFAULT_MOVER_NAME: &str = "kopiur-mover";

/// Default `roleRef.kind` for the mover `RoleBinding` (cluster-scoped install).
pub const DEFAULT_MOVER_ROLE_KIND: &str = "ClusterRole";

/// The operator's own namespace, injected by the chart via the downward API
/// (`fieldRef: metadata.namespace`). Used as the default placement namespace for
/// a `ClusterRepository`'s managed (namespaced) `Maintenance` CR when
/// `spec.maintenance.namespace` is unset. Absent → that placement is unresolved
/// and surfaced as an actionable condition rather than guessed.
pub const OPERATOR_NAMESPACE_ENV: &str = "KOPIUR_NAMESPACE";

/// Override for the writable base directory the controller's in-process kopia
/// uses for its cache/logs/config. Defaults to
/// [`kopiur_kopia::env::DEFAULT_CACHE_DIR`] (`/var/cache/kopia`), where the chart
/// mounts an `emptyDir`; set this only when relocating that mount.
pub const KOPIA_CACHE_DIR_ENV: &str = "KOPIUR_KOPIA_CACHE_DIR";

/// Override for the address the controller's HTTP server (`/metrics`,
/// `/healthz`, `/readyz`) binds to. Unset uses [`HTTP_ADDR`]. Needed on
/// IPv6-only/dual-stack clusters, where the kubelet cannot reach an IPv4-only
/// bind (`0.0.0.0`) and probes never succeed — set `[::]:8081` there. The port
/// must agree with the chart's `controller.probePort` (the Service/probes
/// target that port, not whatever `KOPIUR_HTTP_ADDR` happens to contain).
/// Mirrors the webhook's `KOPIUR_WEBHOOK_ADDR` (`kopiur_webhook::config::WEBHOOK_ADDR_ENV`).
pub const HTTP_ADDR_ENV: &str = "KOPIUR_HTTP_ADDR";

/// Default address the controller's HTTP server (`/metrics`, `/healthz`,
/// `/readyz`) binds to when [`HTTP_ADDR_ENV`] is unset. Matches the chart's
/// `controller.probePort` (8081).
pub const HTTP_ADDR: &str = "0.0.0.0:8081";

/// Resolve the controller HTTP server's bind address from [`HTTP_ADDR_ENV`],
/// falling back to [`HTTP_ADDR`] when unset.
///
/// Unlike [`worker_threads`] (which clamps an out-of-range value rather than
/// fail), an unparseable address is surfaced as an error instead of silently
/// falling back to the default: a typo'd probe address must fail loudly at
/// startup, not silently bind the default and mask the operator's intent
/// (most often an IPv6-only cluster that needed `[::]:8081`).
pub fn http_addr() -> crate::error::Result<std::net::SocketAddr> {
    let value = std::env::var(HTTP_ADDR_ENV).unwrap_or_else(|_| HTTP_ADDR.to_string());
    value
        .parse::<std::net::SocketAddr>()
        .map_err(|source| crate::error::Error::InvalidHttpAddr { value, source })
}

/// Number of tokio worker threads the controller runtime runs. The controller is
/// I/O-bound — watch streams, debounced reconciles, short idempotent kopia calls —
/// so a small fixed pool is ample. The std default (`available_parallelism`) sizes
/// the pool to the HOST core count, NOT the cgroup CPU quota, so on a large node it
/// spawns dozens of worker threads, each carrying a ~2 MiB stack AND a glibc malloc
/// arena that retains freed memory — inflating RSS for no throughput gain. The chart
/// sets this from `controller.workerThreads`; defaults to [`DEFAULT_WORKER_THREADS`].
pub const WORKER_THREADS_ENV: &str = "KOPIUR_WORKER_THREADS";

/// Fallback worker-thread count when [`WORKER_THREADS_ENV`] is unset/unparseable.
/// Two covers the controller's concurrency comfortably; raise it via the chart for
/// a reconcile-heavy deployment.
pub const DEFAULT_WORKER_THREADS: usize = 2;

/// Resolve the tokio worker-thread count from [`WORKER_THREADS_ENV`], clamped to at
/// least 1 (tokio's runtime builder panics on 0), falling back to
/// [`DEFAULT_WORKER_THREADS`] when unset or unparseable.
pub fn worker_threads() -> usize {
    std::env::var(WORKER_THREADS_ENV)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.max(1))
        .unwrap_or(DEFAULT_WORKER_THREADS)
}

/// Opt-in: use the Kubernetes WatchList streaming-list API for the controller's
/// cluster-wide watches, cutting peak memory during the initial list/resync by
/// streaming pages instead of buffering a full page set. Requires apiserver support
/// (the `WatchList` feature: beta in 1.32, GA in 1.34), so it is OFF by default —
/// older clusters are unaffected. The chart exposes it as `controller.streamingLists`.
pub const STREAMING_LISTS_ENV: &str = "KOPIUR_STREAMING_LISTS";

/// Whether [`STREAMING_LISTS_ENV`] is set truthy (`"true"`/`"1"`).
pub fn streaming_lists_enabled() -> bool {
    matches!(
        std::env::var(STREAMING_LISTS_ENV).ok().as_deref(),
        Some("true" | "1")
    )
}

// --- Self-managed webhook TLS (`webhook.tls.mode: self`) --------------------
//
// In `self` mode the controller — not cert-manager — owns the webhook serving
// certificate: it mints a CA + leaf into the serving Secret and injects the CA
// into each webhook configuration's `caBundle` (see [`crate::webhook_tls`]). The
// chart sets these only in `self` mode; absent/false, the controller does no
// webhook-TLS work (cert-manager or a manually-supplied cert is in charge).

/// Gate: when truthy (`"true"`), the controller manages the webhook serving cert.
pub const WEBHOOK_TLS_MANAGED_ENV: &str = "KOPIUR_WEBHOOK_TLS_MANAGED";
/// Name of the `kubernetes.io/tls` Secret the controller mints and the webhook
/// pod mounts. Defaults to [`DEFAULT_WEBHOOK_SECRET_NAME`].
pub const WEBHOOK_SECRET_NAME_ENV: &str = "KOPIUR_WEBHOOK_SECRET_NAME";
/// Name of the webhook `Service` — its DNS name is the leaf cert's SAN.
pub const WEBHOOK_SERVICE_NAME_ENV: &str = "KOPIUR_WEBHOOK_SERVICE_NAME";
/// Name of the `ValidatingWebhookConfiguration` to inject `caBundle` into.
pub const WEBHOOK_VALIDATING_CONFIG_ENV: &str = "KOPIUR_WEBHOOK_VALIDATING_CONFIG";
/// Name of the `MutatingWebhookConfiguration` to inject `caBundle` into.
pub const WEBHOOK_MUTATING_CONFIG_ENV: &str = "KOPIUR_WEBHOOK_MUTATING_CONFIG";

/// Fallback Secret name when [`WEBHOOK_SECRET_NAME_ENV`] is unset; matches the
/// chart's `webhook.tls.secretName` default.
pub const DEFAULT_WEBHOOK_SECRET_NAME: &str = "kopiur-webhook-tls";

/// Steady-state cadence for re-checking the webhook cert for rotation and
/// re-asserting the `caBundle`. The leaf is long-lived and renewed well before
/// expiry, so a slow cadence is ample once the cert is established.
pub const WEBHOOK_TLS_RECONCILE_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(12 * 60 * 60);

/// Retry cadence while webhook TLS setup is still failing (e.g. the webhook
/// configurations aren't registered yet at boot). Fast enough that admission
/// becomes trusted within seconds of the configs appearing, without busy-looping.
pub const WEBHOOK_TLS_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// The OTLP + logging env vars the controller passes through to mover `Job`s,
/// owned by the telemetry crate so the name lists have a single definition.
/// OTLP vars are only forwarded when a collector endpoint is set; the logging
/// vars (`RUST_LOG`, `KOPIUR_LOG_FORMAT`) are forwarded whenever present so a
/// mover inherits the controller's log level and format regardless of OTLP.
pub use kopiur_telemetry::env::{LOG_PASSTHROUGH, OTEL_EXPORTER_OTLP_ENDPOINT, OTLP_PASSTHROUGH};

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // `std::env::set_var`/`remove_var` are process-global; `#[serial]` (already
    // a kopiur-controller dev-dependency) serializes every test in this module
    // that touches HTTP_ADDR_ENV so they can't race each other (Rust runs
    // `#[test]`s concurrently by default). Each test clears the var itself
    // rather than relying on a shared teardown, so a failing assertion can't
    // poison the env for a later test.

    #[test]
    #[serial]
    fn http_addr_unset_uses_default() {
        // SAFETY: serialized by #[serial] against every other test in this module.
        unsafe { std::env::remove_var(HTTP_ADDR_ENV) };
        assert_eq!(http_addr().unwrap(), HTTP_ADDR.parse().unwrap());
    }

    #[test]
    #[serial]
    fn http_addr_custom_value_is_used() {
        // SAFETY: serialized by #[serial] against every other test in this module.
        unsafe { std::env::set_var(HTTP_ADDR_ENV, "[::]:8081") };
        let result = http_addr();
        unsafe { std::env::remove_var(HTTP_ADDR_ENV) };
        assert_eq!(result.unwrap(), "[::]:8081".parse().unwrap());
    }

    #[test]
    #[serial]
    fn http_addr_invalid_value_fails_loudly_with_an_actionable_message() {
        // SAFETY: serialized by #[serial] against every other test in this module.
        unsafe { std::env::set_var(HTTP_ADDR_ENV, "not-an-addr") };
        let result = http_addr();
        unsafe { std::env::remove_var(HTTP_ADDR_ENV) };
        let err = result.expect_err("garbage KOPIUR_HTTP_ADDR must not silently fall back");
        let msg = err.to_string();
        // What: which var, what value. Why: not a valid socket address. Fix:
        // both accepted forms plus how to get back to the default.
        assert!(msg.contains("KOPIUR_HTTP_ADDR='not-an-addr'"), "{msg}");
        assert!(msg.contains("is not a valid socket address"), "{msg}");
        assert!(msg.contains("0.0.0.0:8081"), "{msg}");
        assert!(msg.contains("[::]:8081"), "{msg}");
        assert!(msg.contains("unset it to use the default"), "{msg}");
    }
}
