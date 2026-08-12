//! The shared reconcile [`Context`] handed to every controller.
//!
//! Long kopia operations run in mover `Job`s (the controller writes a
//! `ConfigMap` with a `MoverWorkSpec` and creates a `Job`). The controller only
//! ever spawns kopia directly for short, idempotent ops (repo connect-validate,
//! catalog `snapshot list`, finalizer `snapshot delete`) — and even those run
//! as short-lived Jobs per ADR §5.4. So the [`KopiaClientFactory`] here is a
//! thin builder used only where the design permits in-process invocation; the
//! decision logic is kept pure and unit-tested separately.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use kopiur_api::{Maintenance, Snapshot, SnapshotSchedule};
use kopiur_kopia::{KopiaClient, KopiaClientBuilder, env as kopia_env};
use kube::Client;
use kube::runtime::events::Recorder;
use kube::runtime::reflector::Store;

use crate::metrics::Metrics;

/// Monotonic counter giving every built client a distinct kopia config file.
/// kopia persists the repository binding to `KOPIA_CONFIG_PATH`; concurrent
/// reconciles connecting to *different* repositories must not share one file or
/// they clobber each other's binding. Cache/log dirs are content-addressed and
/// stay shared.
static CONN_SEQ: AtomicU64 = AtomicU64::new(0);

/// Builds short-lived [`KopiaClient`]s for the controller's idempotent ops.
///
/// Holds the cross-cutting defaults: the binary path, the suppress-update env,
/// and the writable base directory kopia uses for its cache/logs/config (an
/// `emptyDir` the chart mounts at [`kopiur_kopia::env::DEFAULT_CACHE_DIR`]).
/// Without redirecting those off `$HOME` (`/nonexistent` on distroless +
/// read-only rootfs), every in-process kopia call fails to create its cache.
/// Per-repository credentials/env are layered on by the caller via [`build`].
///
/// [`build`]: KopiaClientFactory::build
#[derive(Clone, Debug)]
pub struct KopiaClientFactory {
    binary: Option<String>,
    /// Writable base for kopia cache (`<base>/cache`), logs (`<base>/logs`), and
    /// per-connection config files (`<base>/conn-<n>.config`).
    cache_dir: PathBuf,
}

impl Default for KopiaClientFactory {
    fn default() -> Self {
        KopiaClientFactory {
            binary: None,
            cache_dir: PathBuf::from(kopia_env::DEFAULT_CACHE_DIR),
        }
    }
}

impl KopiaClientFactory {
    /// Factory using the default `kopia` binary on `PATH` and the default cache
    /// base ([`kopiur_kopia::env::DEFAULT_CACHE_DIR`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the kopia binary path (injectable for tests / custom images).
    pub fn with_binary(binary: impl Into<String>) -> Self {
        KopiaClientFactory {
            binary: Some(binary.into()),
            ..Self::default()
        }
    }

    /// Override the writable base directory kopia uses for cache/logs/config.
    /// Set from `KOPIUR_KOPIA_CACHE_DIR`; tests point it at a tempdir.
    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = dir.into();
        self
    }

    /// Build a client carrying the given environment (e.g. `KOPIA_PASSWORD`,
    /// S3 credentials). The update check is suppressed globally, and kopia's
    /// cache/log/config dirs are pinned under the factory's writable base — the
    /// config path is unique per call so concurrent connects can't clobber it.
    pub fn build(&self, env: impl IntoIterator<Item = (String, String)>) -> KopiaClient {
        let cache = self.cache_dir.join("cache");
        let logs = self.cache_dir.join("logs");
        // Best-effort: kopia creates these itself, but pre-creating avoids edge
        // cases where it expects the parent to exist. Degrade, never crash — a
        // failure here surfaces as a clear kopia error if the dir is truly
        // unusable. (See [[actionable-error-messages]].)
        for dir in [&cache, &logs] {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::debug!(
                    dir = %dir.display(),
                    error = %e,
                    "could not pre-create kopia cache dir; letting kopia try"
                );
            }
        }
        let config = self.cache_dir.join(format!(
            "conn-{}.config",
            CONN_SEQ.fetch_add(1, Ordering::Relaxed)
        ));

        let mut b: KopiaClientBuilder = KopiaClient::builder()
            .env("KOPIA_CHECK_FOR_UPDATES", "false")
            .env(kopia_env::CACHE_DIRECTORY_ENV, cache.to_string_lossy())
            .env(kopia_env::LOG_DIR_ENV, logs.to_string_lossy())
            .env(kopia_env::CONFIG_PATH_ENV, config.to_string_lossy())
            // Time-bound every subprocess: a hung backend must surface as a
            // retryable kopia error, not pin a reconcile slot forever (see
            // config::KOPIA_SUBPROCESS_TIMEOUT for why this pairs with the
            // reconcile-concurrency cap).
            .default_timeout(crate::config::KOPIA_SUBPROCESS_TIMEOUT);
        if let Some(bin) = &self.binary {
            b = b.binary(bin.clone());
        }
        for (k, v) in env {
            b = b.env(k, v);
        }
        b.build()
    }
}

/// Shared state for all reconcilers. Cheap to `Arc`-wrap and clone: the kube
/// `Client` and the prometheus collectors are internally reference-counted.
#[derive(Clone)]
pub struct Context {
    /// The Kubernetes API client.
    pub client: Client,
    /// A second client from the same inferred config but WITHOUT the read
    /// timeout, used ONLY for the hooks `workloadExec` attach: the exec
    /// WebSocket rides the same timeout-wrapped connector as unary calls, so
    /// the hardened [`crate::config::KUBE_CLIENT_READ_TIMEOUT`] on
    /// [`client`](Self::client) would kill any quiesce command silent for
    /// longer than the window ("returned no status (connection closed
    /// early)"). Exec streams are rare and bounded by the hook timeout, so
    /// exempting them costs nothing.
    pub exec_client: Client,
    /// Factory for short-lived kopia clients (idempotent ops only).
    pub kopia: KopiaClientFactory,
    /// Controller + business metrics.
    pub metrics: Metrics,
    /// Event recorder for surfacing reconcile decisions on the objects.
    pub recorder: Recorder,
    /// Container image used for mover `Job`s (configurable per deployment via
    /// `KOPIUR_MOVER_IMAGE`; defaults to [`crate::jobs::DEFAULT_MOVER_IMAGE`]).
    pub mover_image: String,
    /// Whether [`mover_image`](Self::mover_image) was explicitly configured —
    /// presence drives the `imagePullPolicy` inference in
    /// [`mover_pull_policy`](Self::mover_pull_policy).
    pub mover_image_overridden: bool,
    /// Explicit `imagePullPolicy` for mover Jobs (`KOPIUR_MOVER_PULL_POLICY`);
    /// `None` → inferred (see [`crate::config::effective_mover_pull_policy`]).
    pub mover_pull_policy: Option<crate::config::PullPolicy>,
    /// ServiceAccount the mover `Job` pods run as (configurable via
    /// `KOPIUR_MOVER_SERVICE_ACCOUNT`). The mover PATCHes the owning CR's
    /// `.status`, so this SA must be bound to the operator's status-patch rules.
    /// `None` falls back to the namespace `default` SA (and no minting).
    pub mover_service_account: Option<String>,
    /// Name of the mover `ClusterRole`/`Role` (shipped by the chart) that the
    /// controller-minted per-namespace mover `RoleBinding` references
    /// (`KOPIUR_MOVER_CLUSTERROLE`, default [`crate::config::DEFAULT_MOVER_NAME`]).
    /// Only used when [`mover_service_account`](Self::mover_service_account) is set,
    /// since minting is gated on having a concrete mover SA name.
    pub mover_clusterrole: String,
    /// `roleRef.kind` for the minted mover `RoleBinding` (`ClusterRole` for a
    /// cluster install, `Role` for a namespaced one — `KOPIUR_MOVER_ROLE_KIND`,
    /// default [`crate::config::DEFAULT_MOVER_ROLE_KIND`]). Typed: an invalid
    /// chart value already failed at startup, so reconcilers can't stamp a
    /// roleRef.kind the API server would reject.
    pub mover_role_kind: crate::config::RoleKind,
    /// Env the controller passes through to every mover `Job`: OTLP
    /// (`OTEL_EXPORTER_OTLP_*`, only when a collector is configured) plus logging
    /// (`RUST_LOG`, `KOPIUR_LOG_FORMAT`, whenever set) so movers inherit the
    /// controller's telemetry export and log level/format. `(name, value)` pairs.
    pub mover_env_passthrough: Vec<(String, String)>,
    /// Shared informer cache of all `Maintenance` CRs, reused from the Maintenance
    /// controller's reflector (`Controller::store()`). The `Repository`/
    /// `ClusterRepository` reconcilers read it synchronously to answer "is a
    /// Maintenance configured for me?" without a per-reconcile `Api::list`.
    pub maintenance_store: Store<Maintenance>,
    /// `true` once [`maintenance_store`](Self::maintenance_store) has completed its
    /// initial list (the reflector synced). Until then the maintenance check is
    /// skipped so a cold cache never produces a false "not configured" warning.
    pub maintenance_synced: Arc<AtomicBool>,
    /// The operator's own namespace (`KOPIUR_NAMESPACE`), if known. Used as the
    /// default placement namespace for a `ClusterRepository`'s managed
    /// `Maintenance` CR when `spec.maintenance.namespace` is unset. `None` when
    /// unset (e.g. running out-of-cluster), in which case the cluster-repo
    /// placement is surfaced as unresolved rather than guessed.
    pub operator_namespace: Option<String>,
    /// The install scope (`--cluster-scope` / `--namespace`). Reconcile-time
    /// LISTs of kopiur CRs (e.g. the catalog scan) must match it: a
    /// cluster-wide LIST under a namespaced install's Role RBAC is a permanent
    /// 403 that wedges the reconcile.
    pub watch_scope: crate::config::WatchScope,
    /// Cap on concurrently running Snapshot-delete BATCH mover Jobs, across
    /// every repository (`KOPIUR_MAX_CONCURRENT_DELETE_JOBS`). `None` (the
    /// default) means UNCAPPED — batching (one Job per repository per
    /// accumulation window) is the primary protection against overwhelming
    /// the backend; a cap is an opt-in backstop for a resource-constrained
    /// cluster, and a small default risks head-of-line-blocking every OTHER
    /// repository's deletions behind one slow/failing one. Consulted by the
    /// batch dispatcher's throttle (`crate::snapshot::throttle_verdict`).
    pub max_concurrent_delete_jobs: Option<std::num::NonZeroUsize>,
    /// Shared informer cache of all `Snapshot` CRs, reused from the `Snapshot`
    /// controller's own reflector (`Controller::store()`). `OnceLock` because
    /// the `Context` is built (in `startup::run`) BEFORE `spawn_all` mints the
    /// controllers whose store handles this holds — unlike
    /// [`maintenance_store`](Self::maintenance_store), which `startup::run`
    /// builds and drives itself ahead of `Context::new`. Consumers (M4/M5: the
    /// mass-deletion breaker's per-repo pending count) MUST treat an unset cell
    /// — or one whose [`snapshot_synced`](Self::snapshot_synced) flag hasn't
    /// flipped yet — as "fail safe: requeue, never fire destructive work".
    pub snapshot_store: Arc<OnceLock<Store<Snapshot>>>,
    /// `true` once [`snapshot_store`](Self::snapshot_store) has completed its
    /// initial list (the reflector synced). Until then a cold/absent cache must
    /// never be read as "nothing pending" — see [`snapshot_store`](Self::snapshot_store).
    ///
    /// Flipped by [`mark_snapshot_synced`](Self::mark_snapshot_synced) from the
    /// Snapshot reconcile — a running reconcile is proof the reflector synced (the
    /// applier only dispatches after `store.wait_until_ready()`). This is the
    /// RELIABLE signal: a dedicated `wait_until_ready()` getter on the same
    /// `Controller::store()` races the applier's own getter on kube-rs's
    /// single-waker `DelayedInit` and can hang forever — the bug that left every
    /// external destructive deletion deferred (`StoresNotSynced`) indefinitely.
    pub snapshot_synced: Arc<AtomicBool>,
    /// Shared informer cache of all `SnapshotSchedule` CRs, reused from the
    /// `SnapshotSchedule` controller's own reflector. Same `OnceLock`
    /// construction-order rationale and fail-safe consumption contract as
    /// [`snapshot_store`](Self::snapshot_store).
    pub schedule_store: Arc<OnceLock<Store<SnapshotSchedule>>>,
    /// `true` once [`schedule_store`](Self::schedule_store) has completed its
    /// initial list. Same fail-safe contract as [`snapshot_synced`](Self::snapshot_synced).
    pub schedule_synced: Arc<AtomicBool>,
    /// How long a `Snapshot` parked on a missing DIRECT source PVC
    /// (`SourcePvcAvailable=False`, issue #382) waits before flipping to
    /// terminal `Failed` (`KOPIUR_SOURCE_PVC_DEADLINE_SECONDS`). `None` = park
    /// indefinitely (the explicit `0` escape hatch). See
    /// [`crate::config::SOURCE_PVC_DEADLINE_ENV`].
    pub source_pvc_deadline: Option<std::time::Duration>,
}

impl Context {
    /// Construct a context. The [`Recorder`] should be built from the same
    /// client with a `Reporter` naming this controller.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Client,
        exec_client: Client,
        kopia: KopiaClientFactory,
        metrics: Metrics,
        recorder: Recorder,
        mover_image: String,
        mover_image_overridden: bool,
        mover_pull_policy: Option<crate::config::PullPolicy>,
        mover_service_account: Option<String>,
        mover_clusterrole: String,
        mover_role_kind: crate::config::RoleKind,
        mover_env_passthrough: Vec<(String, String)>,
        maintenance_store: Store<Maintenance>,
        maintenance_synced: Arc<AtomicBool>,
        operator_namespace: Option<String>,
        watch_scope: crate::config::WatchScope,
        max_concurrent_delete_jobs: Option<std::num::NonZeroUsize>,
        source_pvc_deadline: Option<std::time::Duration>,
    ) -> Self {
        Context {
            client,
            exec_client,
            kopia,
            metrics,
            recorder,
            mover_image,
            mover_image_overridden,
            mover_pull_policy,
            mover_service_account,
            mover_clusterrole,
            mover_role_kind,
            mover_env_passthrough,
            maintenance_store,
            maintenance_synced,
            operator_namespace,
            watch_scope,
            max_concurrent_delete_jobs,
            snapshot_store: Arc::new(OnceLock::new()),
            snapshot_synced: Arc::new(AtomicBool::new(false)),
            schedule_store: Arc::new(OnceLock::new()),
            schedule_synced: Arc::new(AtomicBool::new(false)),
            source_pvc_deadline,
        }
    }

    /// Mark the Snapshot informer synced. Called at the top of the Snapshot
    /// reconcile: the Controller's applier only dispatches a reconcile AFTER the
    /// Snapshot reflector's initial LIST completed
    /// (`delay_tasks_until(store.wait_until_ready())`), so a running reconcile is
    /// proof the store is warm — a reliable signal with NO contention on kube-rs's
    /// single-waker `DelayedInit`. Idempotent + cheap (relaxed load, release store
    /// only on the first flip). See [`snapshot_synced`](Self::snapshot_synced).
    pub fn mark_snapshot_synced(&self) {
        mark_synced(&self.snapshot_synced, "Snapshot");
    }

    /// Mark the SnapshotSchedule informer synced, from the SnapshotSchedule
    /// reconcile. Same reliable-signal rationale as
    /// [`mark_snapshot_synced`](Self::mark_snapshot_synced).
    pub fn mark_schedule_synced(&self) {
        mark_synced(&self.schedule_synced, "SnapshotSchedule");
    }

    /// The `imagePullPolicy` to stamp on mover `Job` pods: the explicit
    /// configuration when set, else inferred from whether the mover image was
    /// pinned. Delegates to the pure, unit-tested
    /// [`crate::config::effective_mover_pull_policy`].
    pub fn mover_pull_policy(&self) -> Option<&'static str> {
        crate::config::effective_mover_pull_policy(
            self.mover_pull_policy,
            self.mover_image_overridden,
        )
    }

    /// A `Context` over the given (usually `tower::service_fn` mock) client,
    /// with the Recorder built from the same client and every other field at a
    /// neutral default — for hermetic tests of policy/glue code that needs a
    /// whole `Context` (e.g. `error_policy_for`), not a live cluster.
    #[cfg(test)]
    pub(crate) fn test_context(client: Client) -> Self {
        use kube::runtime::events::Reporter;
        let recorder = Recorder::new(client.clone(), Reporter::from("kopiur-test"));
        Context::new(
            client.clone(),
            client,
            KopiaClientFactory::new(),
            Metrics::new(),
            recorder,
            "example.com/kopiur-mover:test".to_string(),
            false,
            None,
            None,
            crate::config::DEFAULT_MOVER_NAME.to_string(),
            crate::config::DEFAULT_MOVER_ROLE_KIND,
            Vec::new(),
            kube::runtime::reflector::store::<Maintenance>().0,
            Arc::new(AtomicBool::new(false)),
            None,
            crate::config::WatchScope::Cluster,
            None,
            Some(crate::consts::DEFAULT_SOURCE_PVC_DEADLINE),
        )
    }
}

/// Flip an informer-synced flag to `true` (idempotent), logging ONCE on the first
/// flip (`kind` names the informer — the old `publish_synced_store` log line was
/// lost when the flip moved into the reconcile). A relaxed load avoids the release
/// store's write traffic on the (overwhelmingly common) already-synced path; the
/// `swap` behind it fires the log for exactly the one caller that flips
/// `false → true` (its return value is the prior state). Free fn so the
/// load-then-swap is asserted once in [`tests`].
fn mark_synced(flag: &AtomicBool, kind: &str) {
    if !flag.load(Ordering::Relaxed) && !flag.swap(true, Ordering::Release) {
        tracing::info!(kind, "informer cache synced");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- regression (#store-sync): the mass-deletion breaker gates destructive
    // deletion on `*_synced`. It used to be flipped by a spawned
    // `wait_until_ready()` getter on the same `Controller::store()` the applier
    // already awaits — a second getter on kube-rs's single-waker `DelayedInit`
    // that loses the waker race and hangs forever, so the flag never flipped and
    // EVERY external destructive deletion deferred (`StoresNotSynced`) forever. It
    // is now flipped from the reconcile (proof the reflector synced) via this
    // idempotent helper. ---
    #[test]
    fn mark_synced_flips_false_to_true_and_is_idempotent() {
        let flag = AtomicBool::new(false);
        assert!(!flag.load(Ordering::Acquire));
        mark_synced(&flag, "Snapshot");
        assert!(
            flag.load(Ordering::Acquire),
            "the first mark must flip false -> true (the reconcile-driven synced signal)"
        );
        // A second call keeps it true (and takes the cheap already-synced fast path).
        mark_synced(&flag, "Snapshot");
        assert!(
            flag.load(Ordering::Acquire),
            "mark_synced must be idempotent — stay true on repeat reconciles"
        );
    }

    // --- regression: the controller's in-process kopia inherited no writable
    // cache/log/config dir, so on a read-only rootfs with $HOME=/nonexistent
    // every `repository connect/create` failed with
    // `mkdir /nonexistent: read-only file system` and could never persist its
    // config. The factory must pin all three onto the writable base. ---

    fn base() -> PathBuf {
        std::env::temp_dir().join("kopiur-factory-test")
    }

    // --- regression (apiserver-outage EMFILE fix): factory-built kopia clients
    // ran with NO subprocess timeout, so a hung backend (dead NFS mount, stuck
    // object store) pinned its reconcile slot forever. Unbounded reconcile
    // concurrency hid this (only that one object wedged); with the per-controller
    // reconcile cap, a few hung subprocesses would starve a WHOLE controller —
    // the cap and this timeout must exist together. ---
    #[test]
    fn factory_built_clients_carry_the_subprocess_timeout() {
        let client = KopiaClientFactory::new().with_cache_dir(base()).build([]);
        assert_eq!(
            client.default_timeout(),
            Some(crate::config::KOPIA_SUBPROCESS_TIMEOUT),
            "every factory-built kopia client must be time-bounded so a hung \
             backend cannot pin a reconcile slot"
        );
    }

    #[test]
    fn build_injects_kopia_cache_log_and_config_env() {
        let b = base();
        let client = KopiaClientFactory::new().with_cache_dir(&b).build([]);
        let env = client.common_env();

        assert_eq!(
            env.get(kopia_env::CACHE_DIRECTORY_ENV),
            Some(&b.join("cache").to_string_lossy().into_owned())
        );
        assert_eq!(
            env.get(kopia_env::LOG_DIR_ENV),
            Some(&b.join("logs").to_string_lossy().into_owned())
        );
        assert!(
            env.get(kopia_env::CONFIG_PATH_ENV)
                .expect("config path env set")
                .starts_with(&b.join("conn-").to_string_lossy().into_owned()),
            "config path should live under the cache base"
        );
        // The update-check suppression is still applied.
        assert_eq!(
            env.get("KOPIA_CHECK_FOR_UPDATES").map(String::as_str),
            Some("false")
        );
    }

    #[test]
    fn build_gives_each_client_a_distinct_config_path_but_shares_the_cache() {
        let factory = KopiaClientFactory::new().with_cache_dir(base());
        let a = factory.build([]);
        let b = factory.build([]);

        // Per-connection config files must differ so concurrent connects to
        // different repositories can't clobber each other's binding.
        assert_ne!(
            a.common_env().get(kopia_env::CONFIG_PATH_ENV),
            b.common_env().get(kopia_env::CONFIG_PATH_ENV)
        );
        // The content-addressed cache is safe to share.
        assert_eq!(
            a.common_env().get(kopia_env::CACHE_DIRECTORY_ENV),
            b.common_env().get(kopia_env::CACHE_DIRECTORY_ENV)
        );
    }

    #[test]
    fn build_passes_through_caller_env() {
        let client = KopiaClientFactory::new()
            .with_cache_dir(base())
            .build([("KOPIA_PASSWORD".to_string(), "s3cr3t".to_string())]);
        assert_eq!(
            client
                .common_env()
                .get("KOPIA_PASSWORD")
                .map(String::as_str),
            Some("s3cr3t")
        );
    }

    #[test]
    fn default_factory_uses_the_shared_cache_base() {
        let client = KopiaClientFactory::new().build([]);
        assert!(
            client
                .common_env()
                .get(kopia_env::CACHE_DIRECTORY_ENV)
                .expect("cache env set")
                .starts_with(kopia_env::DEFAULT_CACHE_DIR)
        );
    }
}
