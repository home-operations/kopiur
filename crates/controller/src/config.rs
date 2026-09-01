//! The single place for the controller's runtime configuration: the clap
//! surface ([`ControllerArgs`]), the resolved config the rest of the crate
//! consumes ([`ControllerConfig`]), and the names of every environment
//! variable the controller reads. Every knob is a `--flag` with its
//! `KOPIUR_*` env var as fallback (flag > env > default); the env names are
//! the chart contract and must never change. Domain string constants
//! (labels/finalizers/annotations) live in [`crate::consts`]; OTLP env var
//! names are owned by [`kopiur_telemetry::env`] and re-exported here so
//! callers have one import.

use std::net::SocketAddr;

use clap::{ArgAction, Parser};

/// Container image the controller stamps into every mover `Job`. Overrides
/// [`crate::jobs::DEFAULT_MOVER_IMAGE`] when set.
pub const MOVER_IMAGE_ENV: &str = "KOPIUR_MOVER_IMAGE";

/// Explicit `imagePullPolicy` for mover `Job` pods (`Always` / `IfNotPresent`
/// / `Never`). The chart sets it from `image.mover.pullPolicy`. Unset → the
/// policy is inferred: `IfNotPresent` when [`MOVER_IMAGE_ENV`] is set (a
/// pinned, e.g. locally-loaded, image), else the cluster default. See
/// [`effective_mover_pull_policy`].
pub const MOVER_PULL_POLICY_ENV: &str = "KOPIUR_MOVER_PULL_POLICY";

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
pub const DEFAULT_MOVER_ROLE_KIND: RoleKind = RoleKind::ClusterRole;

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
/// `/healthz`, `/readyz`) binds to. Unset uses [`HTTP_ADDR`], which is `[::]`
/// (dual-stack: a wildcard IPv6 bind also accepts IPv4 on Linux when
/// `net.ipv6.bindv6only=0`, the default). Set `0.0.0.0:8081` only on a host
/// where IPv6 is disabled in the pod network namespace, where a `[::]` bind
/// fails outright. The port must agree with the chart's `metrics.port`
/// (the Service/probes target that port, not whatever `KOPIUR_HTTP_ADDR`
/// happens to contain). Mirrors the webhook's `KOPIUR_WEBHOOK_ADDR`
/// (`kopiur_webhook::config::WEBHOOK_ADDR_ENV`).
pub const HTTP_ADDR_ENV: &str = "KOPIUR_HTTP_ADDR";

/// Default address the controller's HTTP server (`/metrics`, `/healthz`,
/// `/readyz`) binds to when [`HTTP_ADDR_ENV`] is unset. Dual-stack `[::]` so a
/// single bind serves both IPv4 and IPv6 kubelets; matches the chart's
/// `metrics.port` (8081).
pub const HTTP_ADDR: &str = "[::]:8081";

/// Number of tokio worker threads the controller runtime runs. The controller is
/// I/O-bound — watch streams, debounced reconciles, short idempotent kopia calls —
/// so a small fixed pool is ample. The std default (`available_parallelism`) sizes
/// the pool to the HOST core count, NOT the cgroup CPU quota, so on a large node it
/// spawns dozens of worker threads, each carrying a ~2 MiB stack AND a glibc malloc
/// arena that retains freed memory — inflating RSS for no throughput gain. The chart
/// sets this from `controller.workerThreads`; defaults to [`DEFAULT_WORKER_THREADS`].
pub const WORKER_THREADS_ENV: &str = "KOPIUR_WORKER_THREADS";

/// Fallback worker-thread count when [`WORKER_THREADS_ENV`] is unset.
/// Two covers the controller's concurrency comfortably; raise it via the chart for
/// a reconcile-heavy deployment.
pub const DEFAULT_WORKER_THREADS: usize = 2;

/// Use the Kubernetes WatchList streaming-list API for the controller's
/// cluster-wide watches, cutting peak memory during the initial list/resync by
/// streaming pages instead of buffering a full page set. Requires apiserver support
/// (the `WatchList` feature: beta in 1.32, GA in 1.34). The env/flag default is
/// off; the chart exposes it as `streamingLists` (default on, gated at startup on
/// the apiserver version — see `startup::effective_streaming_lists`).
pub const STREAMING_LISTS_ENV: &str = "KOPIUR_STREAMING_LISTS";

/// Gate for Lease-based leader election (`--leader-elect`). The chart stamps
/// the flag (from `controller.leaderElection.enabled`); the env var exists for
/// off-chart runs.
pub const LEADER_ELECT_ENV: &str = "KOPIUR_LEADER_ELECT";

/// Name of the leader-election `Lease` in the operator's namespace
/// (`--lease-name`). The chart sets it to the release fullname; defaults to
/// [`DEFAULT_LEASE_NAME`].
pub const LEASE_NAME_ENV: &str = "KOPIUR_LEASE_NAME";

/// Fallback leader-election `Lease` name when [`LEASE_NAME_ENV`] is unset.
pub const DEFAULT_LEASE_NAME: &str = "kopiur-leader";

/// Seconds a held Lease is honored without an observed change. See
/// [`LeaseTimings`]; unset uses [`crate::leader::LEASE_DURATION`].
pub const LEASE_DURATION_ENV: &str = "KOPIUR_LEASE_DURATION_SECONDS";
/// Seconds in the renew WINDOW (a budget for a round of attempts, not one
/// attempt's timeout). Unset uses [`crate::leader::RENEW_DEADLINE`].
pub const RENEW_DEADLINE_ENV: &str = "KOPIUR_RENEW_DEADLINE_SECONDS";
/// Seconds between SUCCESSFUL renew rounds. Unset uses
/// [`crate::leader::RENEW_PERIOD`].
pub const RENEW_PERIOD_ENV: &str = "KOPIUR_RENEW_PERIOD_SECONDS";
/// Seconds between retries inside a renew window, and the standby poll
/// interval. Unset uses [`crate::leader::RETRY_PERIOD`].
pub const RETRY_PERIOD_ENV: &str = "KOPIUR_RETRY_PERIOD_SECONDS";

// --- Self-managed webhook TLS (`webhook.tls.mode: self`) --------------------
//
// In `self` mode the controller — not cert-manager — owns the webhook serving
// certificate: it mints a CA + leaf into the serving Secret and injects the CA
// into each webhook configuration's `caBundle` (see [`crate::webhook_tls`]). The
// chart sets these only in `self` mode; absent/false, the controller does no
// webhook-TLS work (cert-manager or a manually-supplied cert is in charge).

/// Gate: when truthy, the controller manages the webhook serving cert.
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

/// Cadence (seconds) of the orphaned-object sweep ([`crate::sweep`]): reaps
/// mover work-spec ConfigMaps whose Job is already gone (TTL-reaped before the
/// reconciler could observe it, or left behind by operator versions that never
/// deleted them) AND legacy per-run projected credential Secrets left behind
/// by pre-stable-naming versions (#231). `0` disables the sweep.
/// Defaults to [`DEFAULT_WORK_SPEC_SWEEP_INTERVAL_SECS`]. Reachable via the
/// chart's `controller.extraEnv`.
pub const WORK_SPEC_SWEEP_INTERVAL_ENV: &str = "KOPIUR_WORK_SPEC_SWEEP_INTERVAL_SECS";

/// Fallback sweep cadence when [`WORK_SPEC_SWEEP_INTERVAL_ENV`] is unset: 6h.
/// The transition-time cleanup in the reconcilers handles the steady state;
/// the sweep is a backstop, so a slow cadence is ample.
pub const DEFAULT_WORK_SPEC_SWEEP_INTERVAL_SECS: u64 = 6 * 60 * 60;

/// Minimum age (seconds) a sweep victim (work-spec ConfigMap / legacy
/// projected credential Secret) must reach before the sweep may reap it,
/// closing the applied-before-Job window (the objects and the Job are applied
/// in sequence) and any controller-crash-mid-spawn gap.
/// Defaults to [`DEFAULT_WORK_SPEC_SWEEP_MIN_AGE_SECS`]; lower it only in
/// tests. Reachable via the chart's `controller.extraEnv`.
pub const WORK_SPEC_SWEEP_MIN_AGE_ENV: &str = "KOPIUR_WORK_SPEC_SWEEP_MIN_AGE_SECS";

/// Fallback sweep minimum age when [`WORK_SPEC_SWEEP_MIN_AGE_ENV`] is unset: 1h
/// (a spawned run's Job lands within seconds of its ConfigMap; 1h is a
/// comfortable margin over any apply/crash window).
pub const DEFAULT_WORK_SPEC_SWEEP_MIN_AGE_SECS: i64 = 3600;

/// Cap on concurrently RUNNING `Snapshot`-delete BATCH mover Jobs across every
/// repository the controller manages (the operator's own GFS-retention prunes
/// included) — the throttle the batch dispatcher checks before firing a new
/// one (`crate::snapshot::throttle_verdict`). Bounds how much delete traffic a
/// mass-deletion wave (or an incident's retroactive prune) can put on the
/// backend at once; it does NOT bound how many `Snapshot`s one batch Job
/// deletes (see `crate::snapshot::MAX_BATCH_MEMBERS`), nor does it gate
/// whether a deletion is allowed at all — that is
/// `Repository`/`ClusterRepository` `spec.deletionProtection.threshold`.
///
/// `0` (or unset) means UNCAPPED — the default. Batching is the PRIMARY
/// protection here: a mass-deletion wave already drains as one Job per
/// repository per accumulation window (`crate::snapshot::BATCH_QUIET_WINDOW`),
/// not one Job per `Snapshot`, so an operator-wide concurrency cap is an
/// opt-in backstop for a resource-constrained cluster, not a load-bearing
/// safety mechanism — a small default cap risks head-of-line blocking every
/// OTHER repository's deletions behind one slow or failing one. Reachable via
/// the chart's top-level `maxConcurrentDeleteJobs` value.
pub const MAX_CONCURRENT_DELETE_JOBS_ENV: &str = "KOPIUR_MAX_CONCURRENT_DELETE_JOBS";

/// Fallback (raw, `0`-sentinel) cap when [`MAX_CONCURRENT_DELETE_JOBS_ENV`] is
/// unset: uncapped. See [`MAX_CONCURRENT_DELETE_JOBS_ENV`] for why uncapped is
/// the safe default. [`ControllerArgs::resolve`] turns this sentinel into
/// [`ControllerConfig::max_concurrent_delete_jobs`]`: None`.
pub const DEFAULT_MAX_CONCURRENT_DELETE_JOBS: usize = 0;

/// Cluster-wide cap on concurrently running **pooled mover Jobs** — backups,
/// restores and the source side of either replication — across EVERY
/// repository. The same pool a repository's own
/// `spec.concurrency.maxConcurrentJobs` bounds, one level up.
///
/// **The per-repository CRD field is the primary knob**; this is the
/// cluster-operator's backstop for a node pool that cannot host N movers no
/// matter which repositories they belong to. A run must satisfy BOTH: whichever
/// cap it meets first holds it.
///
/// Restores are ALWAYS admitted and never held by either cap (a recovery in
/// progress must not queue behind routine backups); a running restore still
/// occupies a slot, so it displaces backups rather than adding to them.
///
/// `0` (or unset) means UNCAPPED — the default, and today's behavior. Uncapped
/// costs nothing: with neither cap set the gate performs no Job LIST at all
/// (`crate::pool::pool_live_counts`). Reachable via the chart's top-level
/// `maxConcurrentJobs` value.
pub const MAX_CONCURRENT_JOBS_ENV: &str = "KOPIUR_MAX_CONCURRENT_JOBS";

/// Fallback (raw, `0`-sentinel) cap when [`MAX_CONCURRENT_JOBS_ENV`] is unset:
/// uncapped, i.e. every repository's own `spec.concurrency` is the only bound.
/// [`ControllerArgs::resolve`] turns this sentinel into
/// [`ControllerConfig::max_concurrent_jobs`]`: None`.
pub const DEFAULT_MAX_CONCURRENT_JOBS: usize = 0;

/// Per-controller cap on concurrently running reconciles (the operator runs 8
/// controllers, so the process-wide worst case is 8× this). Unlike
/// [`MAX_CONCURRENT_DELETE_JOBS_ENV`] — where batching is the primary
/// protection and uncapped is the safe default — reconcile concurrency has NO
/// other bound: kube-runtime's default (`0` = unlimited) is what let a
/// post-restart re-list dispatch every object's reconcile simultaneously
/// during an apiserver outage, each attempt pinning sockets against the dead
/// endpoint until the process exhausted its fd table (EMFILE) within seconds.
/// Reachable via the chart's top-level `reconcileConcurrency` value.
pub const RECONCILE_CONCURRENCY_ENV: &str = "KOPIUR_RECONCILE_CONCURRENCY";

/// Fallback (raw, `0`-sentinel) per-controller reconcile cap when
/// [`RECONCILE_CONCURRENCY_ENV`] is unset: BOUNDED at 8. Sized so a re-list
/// clears a few-hundred-object fleet in seconds (reconciles are usually
/// sub-second) while capping in-flight API sockets at ≤64 process-wide — and
/// high enough that the slowest slot-holders (hooks, kopia subprocess ops,
/// both minutes-bounded by their own timeouts) can't starve a whole
/// controller. `0` = unbounded (the pre-fix behavior; not recommended).
/// [`ControllerArgs::resolve`] turns the sentinel into
/// [`ControllerConfig::reconcile_concurrency`]`: None`.
pub const DEFAULT_RECONCILE_CONCURRENCY: u16 = 8;

/// Deadline (seconds) for a `Snapshot` parked on a missing DIRECT source PVC
/// (`SourcePvcAvailable=False`, issue #382): measured from the gate
/// condition's `lastTransitionTime`, after which the backup flips to terminal
/// `Failed` so `failedJobsHistoryLimit` bounds it and a `concurrencyPolicy:
/// Forbid` schedule is released. `0` disables the flip (park indefinitely).
/// Defaults to [`crate::consts::DEFAULT_SOURCE_PVC_DEADLINE`] (30 min).
/// Reachable via the chart's `controller.extraEnv`.
pub const SOURCE_PVC_DEADLINE_ENV: &str = "KOPIUR_SOURCE_PVC_DEADLINE_SECONDS";

/// Read timeout for the shared kube client. kube 4.0 defaults `read_timeout`
/// to `None`, so a connection that stalls after establishing (the OOM-flapping
/// apiserver's signature) held its fd until TCP gave up. MUST exceed the watch
/// long-poll window — kube-runtime requests 290s server-side watch timeouts
/// and applies its own 290+5s client idle timeout — or every healthy-but-quiet
/// watch stream would be killed mid-cycle. 295s effective window + 10s
/// headroom. NOTE: the hooks `workloadExec` stream is exempt (it rides
/// [`crate::context::Context::exec_client`], which carries no read timeout) —
/// a quiesce command that is silent for >305s is legitimate there.
pub const KUBE_CLIENT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(305);

/// Read timeout for the DEDICATED leader-election client.
///
/// Election traffic must not ride [`KUBE_CLIENT_READ_TIMEOUT`]. That value is
/// sized for watch long-polls (305s), and `read_timeout` in hyper-timeout is an
/// IDLE timeout on the connection — so on the shared client a wedged HTTP/2
/// connection stays in hyper's pool, silently swallowing requests, for five
/// minutes. Every kopiur request multiplexes over that one connection, lease
/// renewals included: issue #319's "stalled past the renew deadline" storms were
/// renewals queued onto a connection the apiserver had already stopped
/// answering, which is why a FRESH process re-elected in ~12ms.
///
/// Wrapping the renew in a `tokio::time::timeout` cannot fix that — dropping the
/// request future leaves the poisoned connection in the pool, so the retry lands
/// right back on it. Only a TRANSPORT error evicts a connection, and this
/// timeout is what produces one.
///
/// 5s: lease ops are single small round trips and never long-poll, and the
/// renew cadence (`leader::RENEW_PERIOD`, 2s) keeps a healthy connection well
/// inside the idle window. Must stay > RENEW_PERIOD or healthy connections churn.
pub const KUBE_CLIENT_ELECTION_READ_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Connect timeout for the shared kube client (kube 4.0 default: 30s). During
/// the apiserver outage each blackholed connect pinned an fd for the full 30s
/// — the dominant per-fd hold time. In-cluster the apiserver is one ClusterIP
/// hop and a healthy accept is milliseconds; 5s tolerates real blips while
/// cutting the worst-case hold 6×. A too-slow control plane surfaces as a
/// Transient requeue, exactly like today.
pub const KUBE_CLIENT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Cap on concurrently in-flight reconcile-failure Event publishes, process
/// wide. The error policy's publish is fire-and-forget; during an apiserver
/// outage EVERY reconcile fails at once and an unbounded spawn-per-failure was
/// one of the fd-exhaustion amplifiers (each publish opens a socket to the
/// dead endpoint). Failure Events are best-effort observability with
/// Recorder-side series aggregation, so dropping repeats under saturation
/// loses almost nothing; 16 is far above any healthy cluster's concurrent
/// failure-publish rate. Saturation DROPS (debug log + metric), never queues.
pub const MAX_INFLIGHT_FAILURE_EVENT_PUBLISHES: usize = 16;

/// Deadline for a single reconcile-failure Event publish. Bounds the permit
/// (and socket) HOLD TIME — the orthogonal dimension to
/// [`MAX_INFLIGHT_FAILURE_EVENT_PUBLISHES`]'s count bound: a half-alive
/// apiserver can accept the connection and then stall, which would otherwise
/// pin permits until the client write timeout (295s) and silently shrink the
/// pool to zero. A healthy Event POST completes in milliseconds; 10s is
/// generous.
pub const FAILURE_EVENT_PUBLISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Timeout for the controller's short in-process kopia subprocess ops (repo
/// connect-validate, catalog `snapshot list`, finalizer `snapshot delete` —
/// long work runs in mover Jobs). Previously unbounded: a hung backend (dead
/// NFS mount, stuck object store) blocked the subprocess in IO indefinitely
/// and pinned its reconcile slot forever. With the per-controller reconcile
/// cap ([`RECONCILE_CONCURRENCY_ENV`]) a few such hangs would starve a whole
/// controller, so the cap and this bound exist together. 120s is generous for
/// ops that normally complete in seconds, even against a cold/slow backend; a
/// timeout surfaces as a retryable kopia error (Transient requeue).
pub const KOPIA_SUBPROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

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

/// The controller's command-line/environment surface. Every field is a
/// `--flag` with a `KOPIUR_*` env fallback (flag > env > default); the env
/// names are the chart contract. Raw values: empty-string filtering and
/// defaulting happen in [`ControllerArgs::resolve`], because the chart may
/// render an env var as `""` and that has always meant "unset".
#[derive(Debug, Clone, Parser)]
#[command(
    name = "kopiur-controller",
    version,
    about = "Kopiur operator controller: per-CRD reconcilers, finalizers, and scheduling"
)]
pub struct ControllerArgs {
    /// Container image for mover Jobs (unset → the published default image).
    #[arg(long, env = MOVER_IMAGE_ENV)]
    pub mover_image: Option<String>,

    /// imagePullPolicy for mover Jobs: Always, IfNotPresent or Never.
    /// Unset → inferred (IfNotPresent when --mover-image is set, else the
    /// cluster default).
    // Kept a raw string (not a ValueEnum) so an empty chart value keeps
    // meaning "unset"; parsed to `PullPolicy` in `resolve()`.
    #[arg(long, env = MOVER_PULL_POLICY_ENV)]
    pub mover_pull_policy: Option<String>,

    /// ServiceAccount mover Job pods run as; the controller mints it plus a
    /// RoleBinding in each Job's namespace. Unset → the namespace `default` SA
    /// with no minting.
    #[arg(long, env = MOVER_SERVICE_ACCOUNT_ENV)]
    pub mover_service_account: Option<String>,

    /// Name of the mover ClusterRole/Role the minted RoleBinding references.
    #[arg(long, env = MOVER_CLUSTERROLE_ENV, default_value = DEFAULT_MOVER_NAME)]
    pub mover_clusterrole: String,

    /// roleRef.kind for the minted mover RoleBinding: ClusterRole or Role.
    // Kept a raw string (not a ValueEnum) so an empty chart value keeps
    // meaning "default"; parsed to `RoleKind` in `resolve()`.
    #[arg(long, env = MOVER_ROLE_KIND_ENV)]
    pub mover_role_kind: Option<String>,

    /// The operator's own namespace (the chart injects it via the downward API).
    #[arg(long = "operator-namespace", env = OPERATOR_NAMESPACE_ENV)]
    pub operator_namespace: Option<String>,

    /// Writable base dir for the controller's in-process kopia cache/logs/config.
    #[arg(long, env = KOPIA_CACHE_DIR_ENV)]
    pub kopia_cache_dir: Option<String>,

    /// Bind address for the HTTP server (/metrics, /healthz, /readyz).
    #[arg(long, env = HTTP_ADDR_ENV, default_value = HTTP_ADDR, value_parser = parse_http_addr)]
    pub http_addr: SocketAddr,

    /// Tokio worker threads (clamped to at least 1 — tokio panics on 0).
    #[arg(long, env = WORKER_THREADS_ENV, default_value_t = DEFAULT_WORKER_THREADS,
          value_parser = parse_worker_threads)]
    pub worker_threads: usize,

    /// Opt-in: stream cluster-wide list/resync via the WatchList API.
    ///
    /// Not `ArgAction::SetTrue`: that action cannot consume an env value, and
    /// the chart sets `KOPIUR_STREAMING_LISTS=true`/`false`. `num_args = 0..=1`
    /// keeps the bare `--streaming-lists` form working.
    #[arg(long, env = STREAMING_LISTS_ENV, action = ArgAction::Set,
          num_args = 0..=1, default_value_t = false, default_missing_value = "true",
          value_parser = parse_flag_bool)]
    pub streaming_lists: bool,

    /// Gate for self-managed webhook TLS (chart `webhook.tls.mode: self`).
    #[arg(long, env = WEBHOOK_TLS_MANAGED_ENV, action = ArgAction::Set,
          num_args = 0..=1, default_value_t = false, default_missing_value = "true",
          value_parser = parse_flag_bool)]
    pub webhook_tls_managed: bool,

    /// Name of the webhook serving-cert Secret (self-managed TLS).
    #[arg(long, env = WEBHOOK_SECRET_NAME_ENV)]
    pub webhook_secret_name: Option<String>,

    /// Name of the webhook Service; its DNS name is the leaf cert's SAN.
    /// Unset → the secret name.
    #[arg(long, env = WEBHOOK_SERVICE_NAME_ENV)]
    pub webhook_service_name: Option<String>,

    /// ValidatingWebhookConfiguration to inject the caBundle into.
    #[arg(long, env = WEBHOOK_VALIDATING_CONFIG_ENV)]
    pub webhook_validating_config: Option<String>,

    /// MutatingWebhookConfiguration to inject the caBundle into.
    #[arg(long, env = WEBHOOK_MUTATING_CONFIG_ENV)]
    pub webhook_mutating_config: Option<String>,

    /// Lease-based leader election: only the lease holder reconciles.
    /// Required for replicaCount > 1; the chart stamps it from
    /// `controller.leaderElection.enabled`. Needs --operator-namespace (the
    /// Lease lives there) and RBAC on coordination.k8s.io leases.
    #[arg(long, env = LEADER_ELECT_ENV, action = ArgAction::Set,
          num_args = 0..=1, default_value_t = false, default_missing_value = "true",
          value_parser = parse_flag_bool)]
    pub leader_elect: bool,

    /// Name of the leader-election Lease (in the operator's namespace). The
    /// chart sets it to the release fullname so two releases in one namespace
    /// never contend on the same Lease.
    #[arg(long, env = LEASE_NAME_ENV)]
    pub lease_name: Option<String>,

    /// Leader-election protocol timings, in seconds. Unset uses the client-go
    /// defaults; any set that could split-brain is rejected at startup by
    /// [`LeaseTimings::validate`]. Widen these only on a control plane where the
    /// default 10s renew window is genuinely too tight.
    #[arg(long, env = LEASE_DURATION_ENV)]
    pub lease_duration_seconds: Option<u64>,
    /// See [`RENEW_DEADLINE_ENV`].
    #[arg(long, env = RENEW_DEADLINE_ENV)]
    pub renew_deadline_seconds: Option<u64>,
    /// See [`RENEW_PERIOD_ENV`].
    #[arg(long, env = RENEW_PERIOD_ENV)]
    pub renew_period_seconds: Option<u64>,
    /// See [`RETRY_PERIOD_ENV`].
    #[arg(long, env = RETRY_PERIOD_ENV)]
    pub retry_period_seconds: Option<u64>,

    /// Cadence (seconds) of the orphaned work-spec ConfigMap sweep; 0 disables.
    #[arg(long, env = WORK_SPEC_SWEEP_INTERVAL_ENV,
          default_value_t = DEFAULT_WORK_SPEC_SWEEP_INTERVAL_SECS,
          value_parser = parse_sweep_interval)]
    pub work_spec_sweep_interval_secs: u64,

    /// Minimum age (seconds) before the sweep may reap a work-spec ConfigMap.
    #[arg(long, env = WORK_SPEC_SWEEP_MIN_AGE_ENV,
          default_value_t = DEFAULT_WORK_SPEC_SWEEP_MIN_AGE_SECS,
          value_parser = parse_sweep_min_age)]
    pub work_spec_sweep_min_age_secs: i64,

    /// Cap on concurrently running Snapshot-delete BATCH mover Jobs, across
    /// every repository. `0` (the default) means uncapped: batching (one Job
    /// per repository per accumulation window) is the primary protection
    /// against overwhelming the backend; this is an opt-in backstop for a
    /// resource-constrained cluster. Raw `usize`, `0`-sentinel — resolved to
    /// [`ControllerConfig::max_concurrent_delete_jobs`]`: Option<NonZeroUsize>`
    /// in [`ControllerArgs::resolve`].
    #[arg(long, env = MAX_CONCURRENT_DELETE_JOBS_ENV,
          default_value_t = DEFAULT_MAX_CONCURRENT_DELETE_JOBS,
          value_parser = parse_max_concurrent_delete_jobs)]
    pub max_concurrent_delete_jobs: usize,

    /// Cluster-wide cap on concurrently running pooled mover Jobs (backups,
    /// restores, replication sources) across every repository — the backstop
    /// under each repository's own `spec.concurrency.maxConcurrentJobs`. `0`
    /// (the default) means uncapped; restores are always admitted. Raw `usize`,
    /// `0`-sentinel — resolved to
    /// [`ControllerConfig::max_concurrent_jobs`]`: Option<NonZeroUsize>` in
    /// [`ControllerArgs::resolve`].
    #[arg(long, env = MAX_CONCURRENT_JOBS_ENV,
          default_value_t = DEFAULT_MAX_CONCURRENT_JOBS,
          value_parser = parse_max_concurrent_jobs)]
    pub max_concurrent_jobs: usize,

    /// Per-controller cap on concurrently running reconciles. `0` means
    /// unbounded (the pre-fix behavior; not recommended — see
    /// [`RECONCILE_CONCURRENCY_ENV`]). Raw `u16`, `0`-sentinel — resolved to
    /// [`ControllerConfig::reconcile_concurrency`]`: Option<NonZeroU16>` in
    /// [`ControllerArgs::resolve`].
    #[arg(long, env = RECONCILE_CONCURRENCY_ENV,
          default_value_t = DEFAULT_RECONCILE_CONCURRENCY,
          value_parser = parse_reconcile_concurrency)]
    pub reconcile_concurrency: u16,

    /// Seconds a Snapshot parked on a missing DIRECT source PVC waits before
    /// flipping to terminal Failed. `0` disables the flip (park indefinitely).
    /// Raw seconds, `0`-sentinel — resolved to
    /// [`ControllerConfig::source_pvc_deadline`]`: Option<Duration>` in
    /// [`ControllerArgs::resolve`].
    #[arg(long, env = SOURCE_PVC_DEADLINE_ENV,
          default_value_t = crate::consts::DEFAULT_SOURCE_PVC_DEADLINE.as_secs(),
          value_parser = parse_source_pvc_deadline)]
    pub source_pvc_deadline_seconds: u64,

    /// Cluster-scoped install: watch every namespace and reconcile
    /// ClusterRepository. The chart stamps it for `installScope: cluster`.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "namespace")]
    pub cluster_scope: bool,

    /// Namespaced install: watch ONLY this namespace (matches the chart's
    /// Role-only RBAC) and skip cluster-scoped kinds (ClusterRepository,
    /// Namespace referents). The chart stamps `--namespace={{ .Release.Namespace }}`
    /// for `installScope: namespaced`. Deliberately separate from
    /// --operator-namespace / KOPIUR_NAMESPACE (which only places managed
    /// objects and the Lease).
    #[arg(long)]
    pub namespace: Option<String>,
}

/// Which namespaces the controller watches — the install scope, as an enum
/// end-to-end (the type-safety thesis): every watch-building site `match`es
/// this, so a namespaced install structurally cannot register the cluster-wide
/// (Role-RBAC-forbidden) watches that used to leave it silently inert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchScope {
    /// Watch every namespace and reconcile `ClusterRepository`
    /// (`installScope: cluster`, ClusterRole RBAC). Also the default for
    /// off-chart runs, matching the pre-flag behavior.
    Cluster,
    /// Watch exactly this namespace; skip cluster-scoped kinds
    /// (`installScope: namespaced`, Role RBAC).
    Namespaced(String),
}

/// Resolved leader-election settings (present only when `--leader-elect`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderElection {
    /// Name of the `coordination.k8s.io/v1` Lease.
    pub lease_name: String,
    /// Namespace the Lease lives in (the operator's own namespace).
    pub namespace: String,
    /// Protocol timings. Defaults match client-go; see [`LeaseTimings`].
    pub timings: LeaseTimings,
}

/// The four leader-election durations, kept together because they are only
/// meaningful as a set — the no-split-brain property is a relationship BETWEEN
/// them, not a property of any one.
///
/// Configurable because a congested control plane is a cluster property, not a
/// Kopiur one: an operator on a cluster where a 10s renew window is genuinely
/// too tight should be able to widen it without a rebuild (#319). Validated by
/// [`LeaseTimings::validate`] at startup, so no combination that could
/// split-brain is reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseTimings {
    /// How long a held Lease is honored without an observed change before
    /// another replica may claim it.
    pub lease_duration: std::time::Duration,
    /// The renew WINDOW — a budget for a whole round of attempts.
    pub renew_deadline: std::time::Duration,
    /// Cadence between SUCCESSFUL renew rounds (outside the window).
    pub renew_period: std::time::Duration,
    /// Retry cadence inside a renew window, and the standby poll interval.
    pub retry_period: std::time::Duration,
}

/// Ceiling on [`LeaseTimings::lease_duration`]. Generous — 5 minutes is already
/// far beyond any sane election — while keeping the published
/// `leaseDurationSeconds` well inside `i32`, which is the type the Lease API
/// uses and which an unchecked cast would wrap.
pub const MAX_LEASE_DURATION: std::time::Duration = std::time::Duration::from_secs(300);

impl Default for LeaseTimings {
    fn default() -> Self {
        Self {
            lease_duration: crate::leader::LEASE_DURATION,
            renew_deadline: crate::leader::RENEW_DEADLINE,
            renew_period: crate::leader::RENEW_PERIOD,
            retry_period: crate::leader::RETRY_PERIOD,
        }
    }
}

impl LeaseTimings {
    /// Reject any set that could let two replicas both believe they lead.
    ///
    /// The load-bearing check is the first one: worst-case abdication is one
    /// full `renew_period` sleep plus one full `renew_deadline` window, and that
    /// sum — not `renew_deadline` alone — must land inside `lease_duration`.
    /// Getting exactly this wrong is what #319 was.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let worst_case = self.renew_period + self.renew_deadline;
        if worst_case >= self.lease_duration {
            return Err(ConfigError::LeaseTimings(format!(
                "renewPeriod ({:?}) + renewDeadline ({:?}) = {:?} must be strictly less than \
                 leaseDuration ({:?}): a leader can sleep a full renew period and then spend a \
                 full renew window before abdicating, and if that exceeds the lease duration a \
                 standby may claim the Lease while this replica is still reconciling",
                self.renew_period, self.renew_deadline, worst_case, self.lease_duration
            )));
        }
        if self.retry_period >= self.renew_deadline {
            return Err(ConfigError::LeaseTimings(format!(
                "retryPeriod ({:?}) must be less than renewDeadline ({:?}), or a renew window \
                 fits only one attempt and a single slow call ends leadership",
                self.retry_period, self.renew_deadline
            )));
        }
        if self.renew_period.is_zero() || self.retry_period.is_zero() {
            return Err(ConfigError::LeaseTimings(
                "renewPeriod and retryPeriod must be non-zero (a zero cadence busy-loops the \
                 API server)"
                    .to_string(),
            ));
        }
        // Absolute ceiling, not just a relationship. `leaseDurationSeconds` is
        // an i32 on the wire, so an absurd value would silently WRAP to a
        // negative or unrelated duration when serialized — the Lease would then
        // advertise expiry semantics different from the ones this process
        // enforces. Reject it loudly here instead. The bound is also just
        // sanity: leader election measured in hours means a failover takes
        // hours, which no deployment wants.
        if self.lease_duration > MAX_LEASE_DURATION {
            return Err(ConfigError::LeaseTimings(format!(
                "leaseDuration ({:?}) exceeds the {:?} ceiling: a longer lease means a failover \
                 waits that long before any replica may take over, and the value must stay \
                 representable as the i32 `leaseDurationSeconds` the Lease publishes",
                self.lease_duration, MAX_LEASE_DURATION
            )));
        }
        Ok(())
    }
}

/// `roleRef.kind` for the minted mover `RoleBinding`. A closed two-value set,
/// so it is an enum end-to-end (the type-safety thesis): an invalid chart value
/// fails at startup with an actionable message instead of producing a
/// RoleBinding the API server rejects on every mint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleKind {
    /// One shared mover ClusterRole, bound per workload namespace (cluster install).
    ClusterRole,
    /// A mover Role in the operator's namespace (namespaced install).
    Role,
}

impl RoleKind {
    /// The `roleRef.kind` string stamped into the RoleBinding.
    pub fn as_str(self) -> &'static str {
        match self {
            RoleKind::ClusterRole => "ClusterRole",
            RoleKind::Role => "Role",
        }
    }
}

impl std::str::FromStr for RoleKind {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ClusterRole" => Ok(RoleKind::ClusterRole),
            "Role" => Ok(RoleKind::Role),
            _ => Err(ConfigError::InvalidRoleKind { value: s.into() }),
        }
    }
}

/// `imagePullPolicy` for mover `Job` pods — the closed set Kubernetes accepts,
/// as an enum end-to-end (the type-safety thesis): an invalid chart value fails
/// at startup with an actionable message instead of minting Jobs the API server
/// rejects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullPolicy {
    /// Pull on every pod start.
    Always,
    /// Pull only when the image is not already present on the node.
    IfNotPresent,
    /// Never pull; the image must already be present on the node.
    Never,
}

impl PullPolicy {
    /// The `imagePullPolicy` string stamped into the mover Job's pod spec.
    pub fn as_str(self) -> &'static str {
        match self {
            PullPolicy::Always => "Always",
            PullPolicy::IfNotPresent => "IfNotPresent",
            PullPolicy::Never => "Never",
        }
    }
}

impl std::str::FromStr for PullPolicy {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Always" => Ok(PullPolicy::Always),
            "IfNotPresent" => Ok(PullPolicy::IfNotPresent),
            "Never" => Ok(PullPolicy::Never),
            _ => Err(ConfigError::InvalidPullPolicy { value: s.into() }),
        }
    }
}

/// The `imagePullPolicy` the controller stamps on mover `Job` pods: the
/// explicitly configured policy when set, else `IfNotPresent` when the mover
/// image itself was explicitly configured (a pinned — e.g. locally-loaded kind
/// e2e — image must not be re-pulled), else `None` (the cluster default).
///
/// Pure over its inputs so the decision is unit-tested; reconcilers reach it
/// via [`crate::context::Context::mover_pull_policy`].
pub fn effective_mover_pull_policy(
    explicit: Option<PullPolicy>,
    image_overridden: bool,
) -> Option<&'static str> {
    explicit
        .map(PullPolicy::as_str)
        .or_else(|| image_overridden.then_some(PullPolicy::IfNotPresent.as_str()))
}

/// A startup configuration value that parsed as a flag/env but failed
/// cross-field resolution. Structural by definition: the process must fail
/// loudly before any reconciler runs, never guess.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `KOPIUR_MOVER_ROLE_KIND`/`--mover-role-kind` is not one of the two
    /// values Kubernetes accepts as a RoleBinding `roleRef.kind`.
    #[error(
        "KOPIUR_MOVER_ROLE_KIND='{value}' is not a valid mover RoleBinding roleRef.kind; use \
         ClusterRole (cluster-scoped install) or Role (namespaced install); unset it to use the \
         default ClusterRole"
    )]
    InvalidRoleKind {
        /// The raw (unrecognized) value.
        value: String,
    },

    /// `KOPIUR_MOVER_PULL_POLICY`/`--mover-pull-policy` is not one of the
    /// three values Kubernetes accepts as an `imagePullPolicy`.
    #[error(
        "KOPIUR_MOVER_PULL_POLICY='{value}' is not a valid imagePullPolicy for mover Jobs; use \
         Always, IfNotPresent or Never; unset it to infer the policy (IfNotPresent when \
         KOPIUR_MOVER_IMAGE is set, else the cluster default)"
    )]
    InvalidPullPolicy {
        /// The raw (unrecognized) value.
        value: String,
    },

    /// `--leader-elect` without a known operator namespace: the election Lease
    /// must live somewhere, and guessing a namespace could split-brain two
    /// replicas onto different Leases.
    #[error(
        "--leader-elect/KOPIUR_LEADER_ELECT is enabled but the operator namespace is unknown; \
         set KOPIUR_NAMESPACE/--operator-namespace (the chart injects it via the downward API) \
         so the election Lease has a home, or disable leader election"
    )]
    LeaderElectionNeedsNamespace,

    /// A leader-election timing set that could let two replicas both believe
    /// they lead. Rejected at startup rather than discovered during an outage.
    #[error("invalid leader-election timings: {0}")]
    LeaseTimings(String),
}

/// The resolved controller configuration: defaults applied, empty strings
/// filtered, closed value sets parsed to enums. Everything downstream of
/// `main` consumes this — no other code reads the process env for these knobs.
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// Image for mover Jobs (defaulted to [`crate::jobs::DEFAULT_MOVER_IMAGE`]).
    pub mover_image: String,
    /// Whether [`mover_image`](Self::mover_image) was explicitly configured —
    /// presence drives the mover Job `imagePullPolicy` inference.
    pub mover_image_overridden: bool,
    /// Explicit `imagePullPolicy` for mover Jobs; `None` → inferred (see
    /// [`effective_mover_pull_policy`]).
    pub mover_pull_policy: Option<PullPolicy>,
    /// ServiceAccount for mover Job pods; `None` → `default` SA, no minting.
    pub mover_service_account: Option<String>,
    /// Name of the mover ClusterRole/Role the minted RoleBinding references.
    pub mover_clusterrole: String,
    /// `roleRef.kind` for the minted mover RoleBinding.
    pub mover_role_kind: RoleKind,
    /// The operator's own namespace, when known.
    pub operator_namespace: Option<String>,
    /// Override for the in-process kopia cache base; `None` → the default mount.
    pub kopia_cache_dir: Option<String>,
    /// Bind address for the HTTP server (pre-validated by the parser).
    pub http_addr: SocketAddr,
    /// Tokio worker threads (>= 1).
    pub worker_threads: usize,
    /// Stream cluster-wide list/resync via the WatchList API.
    pub streaming_lists: bool,
    /// Self-managed webhook TLS gate.
    pub webhook_tls_managed: bool,
    /// Webhook serving-cert Secret name (defaulted).
    pub webhook_secret_name: String,
    /// Webhook Service name (defaulted to the secret name).
    pub webhook_service_name: String,
    /// ValidatingWebhookConfiguration name, if configured.
    pub webhook_validating_config: Option<String>,
    /// MutatingWebhookConfiguration name, if configured.
    pub webhook_mutating_config: Option<String>,
    /// Which namespaces to watch (install scope).
    pub watch_scope: WatchScope,
    /// Leader election, when enabled (`--leader-elect`).
    pub leader_election: Option<LeaderElection>,
    /// Cadence (seconds) of the orphaned work-spec ConfigMap sweep; 0 disables.
    pub work_spec_sweep_interval_secs: u64,
    /// Minimum age (seconds) before the sweep may reap a work-spec ConfigMap.
    pub work_spec_sweep_min_age_secs: i64,
    /// Cap on concurrently running Snapshot-delete BATCH mover Jobs, across
    /// every repository. `None` (the default) means UNCAPPED — batching is the
    /// primary protection; this is an opt-in backstop. `Option<NonZeroUsize>`
    /// so "no cap" can't be mistaken for "cap of zero" at any call site.
    pub max_concurrent_delete_jobs: Option<std::num::NonZeroUsize>,
    /// Cluster-wide cap on concurrently running POOLED mover Jobs (backup,
    /// restore, replication-source) across every repository — the backstop
    /// beneath each repository's own `spec.concurrency.maxConcurrentJobs`, which
    /// remains the primary knob. `None` (the default) means UNCAPPED, and an
    /// uncapped install pays nothing: the gate skips its Job LIST entirely when
    /// neither cap is set. Restores are never held by either cap.
    /// `Option<NonZeroUsize>` so "no cap" can't be mistaken for "cap of zero" —
    /// which would mean a cluster that never runs another mover.
    pub max_concurrent_jobs: Option<std::num::NonZeroUsize>,
    /// Per-controller cap on concurrently running reconciles. `None` means
    /// UNBOUNDED (the explicit `0` escape hatch — the pre-fix behavior, not
    /// recommended); the default is BOUNDED at
    /// [`DEFAULT_RECONCILE_CONCURRENCY`]. `Option<NonZeroU16>` so "no cap"
    /// can't be mistaken for "cap of zero" at any call site.
    pub reconcile_concurrency: Option<std::num::NonZeroU16>,
    /// How long a `Snapshot` parked on a missing DIRECT source PVC waits (from
    /// the `SourcePvcAvailable=False` condition's `lastTransitionTime`) before
    /// flipping to terminal `Failed`. `None` = the explicit `0` escape hatch:
    /// park indefinitely, never flip. `Option<Duration>` so "no deadline" can't
    /// be mistaken for "deadline of zero" at any call site.
    pub source_pvc_deadline: Option<std::time::Duration>,
}

impl ControllerArgs {
    /// Resolve the raw flag/env surface into a [`ControllerConfig`]: filter
    /// empty strings (a chart-rendered `""` has always meant "unset"), apply
    /// defaults, parse closed value sets, clamp the worker-thread count.
    pub fn resolve(self) -> Result<ControllerConfig, ConfigError> {
        fn nonempty(v: Option<String>) -> Option<String> {
            v.filter(|s| !s.is_empty())
        }

        let mover_image = nonempty(self.mover_image);
        let mover_image_overridden = mover_image.is_some();
        let mover_pull_policy = nonempty(self.mover_pull_policy)
            .map(|v| v.parse::<PullPolicy>())
            .transpose()?;
        let mover_role_kind = match nonempty(self.mover_role_kind) {
            Some(v) => v.parse::<RoleKind>()?,
            None => DEFAULT_MOVER_ROLE_KIND,
        };
        let mover_clusterrole = if self.mover_clusterrole.is_empty() {
            DEFAULT_MOVER_NAME.to_string()
        } else {
            self.mover_clusterrole
        };
        let webhook_secret_name = nonempty(self.webhook_secret_name)
            .unwrap_or_else(|| DEFAULT_WEBHOOK_SECRET_NAME.to_string());
        let webhook_service_name =
            nonempty(self.webhook_service_name).unwrap_or_else(|| webhook_secret_name.clone());

        let operator_namespace = nonempty(self.operator_namespace);

        // Install scope: --cluster-scope and --namespace are mutually exclusive
        // at parse time (clap `conflicts_with`); with neither (an off-chart
        // run), default to cluster-wide — the pre-flag behavior.
        let watch_scope = match nonempty(self.namespace) {
            Some(ns) => WatchScope::Namespaced(ns),
            None => WatchScope::Cluster,
        };

        // Leader election needs a namespace for its Lease; guessing one could
        // split-brain two replicas onto different Leases, so fail loudly.
        let leader_election = if self.leader_elect {
            let defaults = LeaseTimings::default();
            let secs = |v: Option<u64>, d: std::time::Duration| {
                v.map(std::time::Duration::from_secs).unwrap_or(d)
            };
            let timings = LeaseTimings {
                lease_duration: secs(self.lease_duration_seconds, defaults.lease_duration),
                renew_deadline: secs(self.renew_deadline_seconds, defaults.renew_deadline),
                renew_period: secs(self.renew_period_seconds, defaults.renew_period),
                retry_period: secs(self.retry_period_seconds, defaults.retry_period),
            };
            // Fail at startup, not at the first stalled renew: a set that
            // violates the margin cannot be allowed to run at all.
            timings.validate()?;
            Some(LeaderElection {
                lease_name: nonempty(self.lease_name)
                    .unwrap_or_else(|| DEFAULT_LEASE_NAME.to_string()),
                namespace: operator_namespace
                    .clone()
                    .ok_or(ConfigError::LeaderElectionNeedsNamespace)?,
                timings,
            })
        } else {
            None
        };

        Ok(ControllerConfig {
            mover_image: mover_image
                .unwrap_or_else(|| crate::jobs::DEFAULT_MOVER_IMAGE.to_string()),
            mover_image_overridden,
            mover_pull_policy,
            mover_service_account: nonempty(self.mover_service_account),
            mover_clusterrole,
            mover_role_kind,
            operator_namespace,
            kopia_cache_dir: nonempty(self.kopia_cache_dir),
            http_addr: self.http_addr,
            worker_threads: self.worker_threads.max(1),
            streaming_lists: self.streaming_lists,
            webhook_tls_managed: self.webhook_tls_managed,
            webhook_secret_name,
            webhook_service_name,
            webhook_validating_config: nonempty(self.webhook_validating_config),
            webhook_mutating_config: nonempty(self.webhook_mutating_config),
            watch_scope,
            leader_election,
            work_spec_sweep_interval_secs: self.work_spec_sweep_interval_secs,
            work_spec_sweep_min_age_secs: self.work_spec_sweep_min_age_secs.max(0),
            // `0` ⇔ `None` (uncapped) — the whole point of `NonZeroUsize::new`.
            max_concurrent_delete_jobs: std::num::NonZeroUsize::new(
                self.max_concurrent_delete_jobs,
            ),
            // `0` ⇔ `None` (uncapped): the per-repository cap is the only bound.
            max_concurrent_jobs: std::num::NonZeroUsize::new(self.max_concurrent_jobs),
            reconcile_concurrency: std::num::NonZeroU16::new(self.reconcile_concurrency),
            // `0` ⇔ `None` (park indefinitely, never flip to Failed).
            source_pvc_deadline: (self.source_pvc_deadline_seconds > 0)
                .then(|| std::time::Duration::from_secs(self.source_pvc_deadline_seconds)),
        })
    }
}

/// Value parser for [`HTTP_ADDR_ENV`]/`--http-addr`. A typo'd probe address
/// must fail loudly at startup, not silently bind the default and mask the
/// operator's intent (most often a host with IPv6 disabled that needed
/// `0.0.0.0:8081`), so the message carries the what/why/fix in full. An EMPTY
/// value means "unset" (the default) — the chart can render an env var as `""`
/// (e.g. a nulled Helm value through `| quote`), and clap consults the env
/// before `resolve()`'s empty-string filter can run.
fn parse_http_addr(value: &str) -> Result<SocketAddr, String> {
    let value = if value.is_empty() { HTTP_ADDR } else { value };
    value.parse::<SocketAddr>().map_err(|_| {
        format!(
            "KOPIUR_HTTP_ADDR='{value}' is not a valid socket address; use host:port, e.g. \
             [::]:8081 (IPv6/dual-stack, the default), 0.0.0.0:8081 (IPv4-only, for hosts with \
             IPv6 disabled); unset it to use the default [::]:8081"
        )
    })
}

/// Value parser for [`WORK_SPEC_SWEEP_INTERVAL_ENV`]. An empty value means
/// "unset" (the chart can render an env var as `""`); `0` disables the sweep.
fn parse_sweep_interval(value: &str) -> Result<u64, String> {
    if value.is_empty() {
        return Ok(DEFAULT_WORK_SPEC_SWEEP_INTERVAL_SECS);
    }
    value.parse::<u64>().map_err(|_| {
        format!(
            "KOPIUR_WORK_SPEC_SWEEP_INTERVAL_SECS='{value}' is not a valid interval; use a \
             number of seconds (0 disables the sweep); unset it to use the default \
             {DEFAULT_WORK_SPEC_SWEEP_INTERVAL_SECS}"
        )
    })
}

/// Value parser for [`WORK_SPEC_SWEEP_MIN_AGE_ENV`]. An empty value means
/// "unset"; negative values are clamped to 0 in `resolve()`.
fn parse_sweep_min_age(value: &str) -> Result<i64, String> {
    if value.is_empty() {
        return Ok(DEFAULT_WORK_SPEC_SWEEP_MIN_AGE_SECS);
    }
    value.parse::<i64>().map_err(|_| {
        format!(
            "KOPIUR_WORK_SPEC_SWEEP_MIN_AGE_SECS='{value}' is not a valid age; use a number of \
             seconds; unset it to use the default {DEFAULT_WORK_SPEC_SWEEP_MIN_AGE_SECS}"
        )
    })
}

/// Value parser for [`MAX_CONCURRENT_DELETE_JOBS_ENV`]. Empty means "unset" (→
/// the default, uncapped); `0` is explicitly valid and ALSO means uncapped —
/// matching this repo's `threshold: 0`-disables idiom
/// (`kopiur_api::consts::effective_mass_deletion_threshold`). Only garbage
/// (not a non-negative integer) is rejected.
fn parse_max_concurrent_delete_jobs(value: &str) -> Result<usize, String> {
    if value.is_empty() {
        return Ok(DEFAULT_MAX_CONCURRENT_DELETE_JOBS);
    }
    value.parse::<usize>().map_err(|_| {
        format!(
            "KOPIUR_MAX_CONCURRENT_DELETE_JOBS='{value}' is not a valid job count; use a \
             non-negative integer (0 = uncapped, the default); unset it to use the default \
             {DEFAULT_MAX_CONCURRENT_DELETE_JOBS}"
        )
    })
}

/// Value parser for [`MAX_CONCURRENT_JOBS_ENV`]. Empty means "unset" (→ the
/// default, uncapped); `0` is explicitly valid and ALSO means uncapped, matching
/// the CRD field it backstops (`kopiur_api::consts::effective_max_concurrent_jobs`
/// collapses `0` to `None` for exactly the same reason). Only garbage (not a
/// non-negative integer) is rejected — loudly, because silently defaulting a
/// typo'd cap to "uncapped" would leave an operator believing a backstop is in
/// place that is not.
fn parse_max_concurrent_jobs(value: &str) -> Result<usize, String> {
    if value.is_empty() {
        return Ok(DEFAULT_MAX_CONCURRENT_JOBS);
    }
    value.parse::<usize>().map_err(|_| {
        format!(
            "KOPIUR_MAX_CONCURRENT_JOBS='{value}' is not a valid job count; use a non-negative \
             integer (0 = uncapped, the default) — this is the CLUSTER-WIDE backstop under each \
             repository's spec.concurrency.maxConcurrentJobs; unset it to use the default \
             {DEFAULT_MAX_CONCURRENT_JOBS}"
        )
    })
}

/// Value parser for [`RECONCILE_CONCURRENCY_ENV`]. Empty means "unset" (→ the
/// BOUNDED default — falling back to unbounded here would silently reopen the
/// EMFILE hole the default exists to close); `0` is explicitly valid and means
/// unbounded (the deliberate escape hatch). Only garbage is rejected.
fn parse_reconcile_concurrency(value: &str) -> Result<u16, String> {
    if value.is_empty() {
        return Ok(DEFAULT_RECONCILE_CONCURRENCY);
    }
    value.parse::<u16>().map_err(|_| {
        format!(
            "KOPIUR_RECONCILE_CONCURRENCY='{value}' is not a valid per-controller reconcile \
             cap; use an integer 0-65535 (0 = unbounded, not recommended), e.g. 8; unset it \
             to use the default {DEFAULT_RECONCILE_CONCURRENCY}"
        )
    })
}

/// Value parser for [`SOURCE_PVC_DEADLINE_ENV`]. Empty means "unset" (→ the
/// bounded default); `0` is explicitly valid and means "park indefinitely,
/// never flip to Failed" — the repo's `0`-disables idiom. Only garbage is
/// rejected, loudly.
fn parse_source_pvc_deadline(value: &str) -> Result<u64, String> {
    if value.is_empty() {
        return Ok(crate::consts::DEFAULT_SOURCE_PVC_DEADLINE.as_secs());
    }
    value.parse::<u64>().map_err(|_| {
        format!(
            "{SOURCE_PVC_DEADLINE_ENV}='{value}' is not a valid deadline; use a number of \
             seconds (0 = park indefinitely, never fail); unset it to use the default {}",
            crate::consts::DEFAULT_SOURCE_PVC_DEADLINE.as_secs()
        )
    })
}

/// Value parser for [`WORKER_THREADS_ENV`]/`--worker-threads`: empty ≡ unset
/// (→ the default, matching the pre-clap tolerance for a blanked env var);
/// garbage still fails loudly with the what/why/fix.
fn parse_worker_threads(value: &str) -> Result<usize, String> {
    if value.is_empty() {
        return Ok(DEFAULT_WORKER_THREADS);
    }
    value.parse::<usize>().map_err(|_| {
        format!(
            "KOPIUR_WORKER_THREADS='{value}' is not a valid thread count; use a positive \
             integer, e.g. 2; unset it to use the default {DEFAULT_WORKER_THREADS}"
        )
    })
}

/// Boolean parser for flag-or-env fields: the boolish value set
/// (true/1/yes/on & false/0/no/off, case-insensitive), plus empty ≡ unset
/// (→ false, every bool here defaults off) so a chart-rendered `""` keeps
/// meaning "not enabled" instead of aborting the process at parse time.
fn parse_flag_bool(value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "" => Ok(false),
        "true" | "t" | "yes" | "y" | "on" | "1" => Ok(true),
        "false" | "f" | "no" | "n" | "off" | "0" => Ok(false),
        _ => Err(format!(
            "'{value}' is not a valid boolean; use true/false (also accepted: 1/0, yes/no, \
             on/off); unset it or leave it empty for the default"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // Every test that PARSES is `#[serial]`, not only the two that mutate the
    // env: clap consults the process env for every `env = ...` field on every
    // parse, so an unmarked parsing test could observe the env-mutating tests'
    // vars mid-flight (`set_var` is process-global; Rust runs tests
    // concurrently by default). This is the repo's established env-test idiom.

    fn parse(args: &[&str]) -> ControllerArgs {
        ControllerArgs::try_parse_from(
            std::iter::once("kopiur-controller").chain(args.iter().copied()),
        )
        .expect("args must parse")
    }

    fn resolve(args: &[&str]) -> ControllerConfig {
        parse(args).resolve().expect("config must resolve")
    }

    #[test]
    #[serial]
    fn defaults_match_the_documented_contract() {
        let cfg = resolve(&[]);
        assert_eq!(cfg.mover_image, crate::jobs::DEFAULT_MOVER_IMAGE);
        assert!(!cfg.mover_image_overridden);
        assert_eq!(cfg.mover_pull_policy, None);
        assert_eq!(cfg.mover_service_account, None);
        assert_eq!(cfg.mover_clusterrole, DEFAULT_MOVER_NAME);
        assert_eq!(cfg.mover_role_kind, RoleKind::ClusterRole);
        assert_eq!(cfg.operator_namespace, None);
        assert_eq!(cfg.kopia_cache_dir, None);
        assert_eq!(cfg.http_addr, HTTP_ADDR.parse().unwrap());
        assert_eq!(cfg.worker_threads, DEFAULT_WORKER_THREADS);
        assert!(!cfg.streaming_lists);
        assert!(!cfg.webhook_tls_managed);
        assert_eq!(cfg.webhook_secret_name, DEFAULT_WEBHOOK_SECRET_NAME);
        assert_eq!(cfg.webhook_service_name, DEFAULT_WEBHOOK_SECRET_NAME);
        // Off-chart runs keep the pre-flag behavior: cluster-wide, no election.
        assert_eq!(cfg.watch_scope, WatchScope::Cluster);
        assert_eq!(cfg.leader_election, None);
        // Uncapped by default: batching is the primary protection.
        assert_eq!(cfg.max_concurrent_delete_jobs, None);
        // BOUNDED by default: reconcile concurrency has no other bound (the
        // apiserver-outage EMFILE fix).
        assert_eq!(
            cfg.reconcile_concurrency,
            std::num::NonZeroU16::new(DEFAULT_RECONCILE_CONCURRENCY)
        );
        // A missing DIRECT source PVC parks the Snapshot, then fails it
        // terminally after 30 minutes by default (issue #382).
        assert_eq!(
            cfg.source_pvc_deadline,
            Some(crate::consts::DEFAULT_SOURCE_PVC_DEADLINE)
        );
    }

    #[test]
    #[serial]
    fn source_pvc_deadline_resolves_seconds_and_zero_means_indefinite() {
        // Explicit seconds → the bounded deadline.
        let cfg = resolve(&["--source-pvc-deadline-seconds", "90"]);
        assert_eq!(
            cfg.source_pvc_deadline,
            Some(std::time::Duration::from_secs(90))
        );
        // `0` disables the terminal flip (park indefinitely) — the repo's
        // `0`-disables idiom (preflight `resolve_timeout`, the sweep interval).
        let cfg = resolve(&["--source-pvc-deadline-seconds", "0"]);
        assert_eq!(cfg.source_pvc_deadline, None);
    }

    #[test]
    #[serial]
    fn source_pvc_deadline_rejects_garbage_actionably() {
        let err = ControllerArgs::try_parse_from([
            "kopiur-controller",
            "--source-pvc-deadline-seconds",
            "soon",
        ])
        .expect_err("garbage must fail loudly");
        let msg = err.to_string();
        assert!(msg.contains(SOURCE_PVC_DEADLINE_ENV), "{msg}");
        assert!(msg.contains("seconds"), "{msg}");
    }

    #[test]
    #[serial]
    fn every_flag_round_trips() {
        let cfg = resolve(&[
            "--mover-image",
            "example.com/mover:v1",
            "--mover-service-account",
            "mover-sa",
            "--mover-clusterrole",
            "my-role",
            "--mover-role-kind",
            "Role",
            "--operator-namespace",
            "kopiur-system",
            "--kopia-cache-dir",
            "/scratch/kopia",
            "--http-addr",
            "[::]:9090",
            "--worker-threads",
            "4",
            "--streaming-lists",
            "true",
            "--webhook-tls-managed",
            "true",
            "--webhook-secret-name",
            "tls-secret",
            "--webhook-service-name",
            "webhook-svc",
            "--webhook-validating-config",
            "vwc",
            "--webhook-mutating-config",
            "mwc",
            "--max-concurrent-delete-jobs",
            "4",
            "--reconcile-concurrency",
            "12",
        ]);
        assert_eq!(cfg.mover_image, "example.com/mover:v1");
        assert!(cfg.mover_image_overridden);
        assert_eq!(cfg.mover_service_account.as_deref(), Some("mover-sa"));
        assert_eq!(cfg.mover_clusterrole, "my-role");
        assert_eq!(cfg.mover_role_kind, RoleKind::Role);
        assert_eq!(cfg.operator_namespace.as_deref(), Some("kopiur-system"));
        assert_eq!(cfg.kopia_cache_dir.as_deref(), Some("/scratch/kopia"));
        assert_eq!(cfg.http_addr, "[::]:9090".parse().unwrap());
        assert_eq!(cfg.worker_threads, 4);
        assert!(cfg.streaming_lists);
        assert!(cfg.webhook_tls_managed);
        assert_eq!(cfg.webhook_secret_name, "tls-secret");
        assert_eq!(cfg.webhook_service_name, "webhook-svc");
        assert_eq!(cfg.webhook_validating_config.as_deref(), Some("vwc"));
        assert_eq!(cfg.webhook_mutating_config.as_deref(), Some("mwc"));
        assert_eq!(
            cfg.max_concurrent_delete_jobs,
            std::num::NonZeroUsize::new(4)
        );
        assert_eq!(cfg.reconcile_concurrency, std::num::NonZeroU16::new(12));
    }

    // --- the chart argv contract: deployment.tpl has stamped these args since
    // v0.1 (a strict parser that rejected them would crash-loop every deployed
    // controller on upgrade) — and they now DO something, so the resolved
    // semantics are pinned here too. ---

    #[test]
    #[serial]
    fn chart_stamped_args_resolve_cluster_install() {
        let cfg = parse(&[
            "--leader-elect=true",
            "--cluster-scope",
            "--operator-namespace",
            "kopiur-system",
        ])
        .resolve()
        .expect("cluster-install argv must resolve");
        assert_eq!(cfg.watch_scope, WatchScope::Cluster);
        assert_eq!(
            cfg.leader_election,
            Some(LeaderElection {
                lease_name: DEFAULT_LEASE_NAME.to_string(),
                namespace: "kopiur-system".to_string(),
                timings: LeaseTimings::default(),
            })
        );
    }

    #[test]
    #[serial]
    fn chart_stamped_args_resolve_namespaced_install() {
        let cfg = parse(&["--leader-elect=false", "--namespace=kopiur-system"])
            .resolve()
            .expect("namespaced-install argv must resolve");
        assert_eq!(
            cfg.watch_scope,
            WatchScope::Namespaced("kopiur-system".to_string())
        );
        assert_eq!(cfg.leader_election, None);
    }

    #[test]
    #[serial]
    fn cluster_scope_and_namespace_conflict_at_parse_time() {
        // The chart renders exactly one of the two; passing both by hand is a
        // contradiction the parser must reject, not resolve by precedence.
        ControllerArgs::try_parse_from([
            "kopiur-controller",
            "--cluster-scope",
            "--namespace=kopiur-system",
        ])
        .expect_err("--cluster-scope with --namespace must be rejected");
    }

    #[test]
    #[serial]
    fn leader_election_without_a_namespace_fails_actionably() {
        let err = parse(&["--leader-elect=true"])
            .resolve()
            .expect_err("leader election must not guess a Lease namespace");
        let msg = err.to_string();
        assert!(msg.contains("KOPIUR_NAMESPACE"), "{msg}");
        assert!(msg.contains("Lease"), "{msg}");
        assert!(msg.contains("disable leader election"), "{msg}");
    }

    #[test]
    #[serial]
    fn lease_name_flag_overrides_the_default() {
        let cfg = parse(&[
            "--leader-elect",
            "--operator-namespace=ns1",
            "--lease-name=myrelease-kopiur",
        ])
        .resolve()
        .expect("must resolve");
        assert_eq!(
            cfg.leader_election.expect("elected").lease_name,
            "myrelease-kopiur"
        );
    }

    // --- empty-string filtering: the chart may render an env var as "" and
    // that has always meant "unset" (lib.rs used `.filter(|s| !s.is_empty())`).
    // clap treats an empty env value as present, so resolve() must filter. ---

    #[test]
    #[serial]
    fn empty_values_mean_unset() {
        let cfg = resolve(&[
            "--mover-image=",
            "--mover-service-account=",
            "--mover-clusterrole=",
            "--mover-role-kind=",
            "--operator-namespace=",
            "--kopia-cache-dir=",
            "--webhook-secret-name=",
            "--webhook-service-name=",
            "--webhook-validating-config=",
            "--webhook-mutating-config=",
            "--namespace=",
            "--lease-name=",
            "--max-concurrent-delete-jobs=",
        ]);
        assert_eq!(cfg.mover_image, crate::jobs::DEFAULT_MOVER_IMAGE);
        assert!(!cfg.mover_image_overridden);
        assert_eq!(cfg.mover_service_account, None);
        assert_eq!(cfg.mover_clusterrole, DEFAULT_MOVER_NAME);
        assert_eq!(cfg.mover_role_kind, RoleKind::ClusterRole);
        assert_eq!(cfg.operator_namespace, None);
        assert_eq!(cfg.kopia_cache_dir, None);
        assert_eq!(cfg.webhook_secret_name, DEFAULT_WEBHOOK_SECRET_NAME);
        assert_eq!(cfg.webhook_service_name, DEFAULT_WEBHOOK_SECRET_NAME);
        assert_eq!(cfg.webhook_validating_config, None);
        assert_eq!(cfg.webhook_mutating_config, None);
        // An empty --namespace means "no narrowing", not Namespaced("").
        assert_eq!(cfg.watch_scope, WatchScope::Cluster);
        assert_eq!(cfg.max_concurrent_delete_jobs, None);
    }

    #[test]
    #[serial]
    fn webhook_service_name_falls_back_to_the_secret_name() {
        let cfg = resolve(&["--webhook-secret-name", "custom-tls"]);
        assert_eq!(cfg.webhook_service_name, "custom-tls");
    }

    #[test]
    #[serial]
    fn role_kind_rejects_garbage_with_an_actionable_message() {
        let err = parse(&["--mover-role-kind", "SuperRole"])
            .resolve()
            .expect_err("garbage role kind must not silently default");
        let msg = err.to_string();
        // What: which var, what value. Why: not a valid kind. Fix: the two
        // valid values plus how to get back to the default.
        assert!(msg.contains("KOPIUR_MOVER_ROLE_KIND='SuperRole'"), "{msg}");
        assert!(msg.contains("ClusterRole"), "{msg}");
        assert!(msg.contains("Role (namespaced install)"), "{msg}");
        assert!(msg.contains("unset it to use the default"), "{msg}");
    }

    // --- the mover imagePullPolicy decision (KOPIUR_MOVER_PULL_POLICY): the
    // chart set this env var long before the controller read it; these pin the
    // now-wired semantics — explicit wins, presence-inference is the fallback,
    // unset+default-image leaves the cluster default in charge. ---

    #[test]
    #[serial]
    fn pull_policy_explicit_wins_over_inference() {
        assert_eq!(
            effective_mover_pull_policy(Some(PullPolicy::Always), true),
            Some("Always")
        );
        assert_eq!(
            effective_mover_pull_policy(Some(PullPolicy::Never), true),
            Some("Never")
        );
        assert_eq!(
            effective_mover_pull_policy(Some(PullPolicy::IfNotPresent), false),
            Some("IfNotPresent")
        );
    }

    #[test]
    #[serial]
    fn pull_policy_unset_infers_from_image_override() {
        // A pinned (e.g. locally-loaded kind e2e) image must not be re-pulled.
        assert_eq!(
            effective_mover_pull_policy(None, true),
            Some("IfNotPresent")
        );
        // Default image, no explicit policy → the cluster default decides.
        assert_eq!(effective_mover_pull_policy(None, false), None);
    }

    #[test]
    #[serial]
    fn pull_policy_flag_parses_and_empty_means_unset() {
        let cfg = resolve(&["--mover-pull-policy", "Always"]);
        assert_eq!(cfg.mover_pull_policy, Some(PullPolicy::Always));
        // The chart quotes `image.mover.pullPolicy`; an explicit "" keeps
        // meaning "unset" like every other empty-rendered env value.
        assert_eq!(resolve(&["--mover-pull-policy="]).mover_pull_policy, None);
    }

    #[test]
    #[serial]
    fn pull_policy_rejects_garbage_with_an_actionable_message() {
        let err = parse(&["--mover-pull-policy", "Sometimes"])
            .resolve()
            .expect_err("garbage pull policy must not silently default");
        let msg = err.to_string();
        assert!(
            msg.contains("KOPIUR_MOVER_PULL_POLICY='Sometimes'"),
            "{msg}"
        );
        assert!(msg.contains("Always, IfNotPresent or Never"), "{msg}");
        assert!(msg.contains("unset it to infer the policy"), "{msg}");
    }

    #[test]
    #[serial]
    fn empty_typed_values_mean_unset_too() {
        // The chart can render ANY env var as "" (a nulled Helm value through
        // `| quote`); typed fields must treat that as unset — clap consults the
        // env before resolve()'s empty-string filter can run, so the tolerance
        // lives in the value parsers. Regression guard for the upgrade-breaking
        // "cannot parse integer from empty string" abort.
        let cfg = resolve(&[
            "--worker-threads=",
            "--http-addr=",
            "--streaming-lists=",
            "--webhook-tls-managed=",
            "--leader-elect=",
        ]);
        assert_eq!(cfg.worker_threads, DEFAULT_WORKER_THREADS);
        assert_eq!(cfg.http_addr, HTTP_ADDR.parse().unwrap());
        assert!(!cfg.streaming_lists);
        assert!(!cfg.webhook_tls_managed);
        assert_eq!(cfg.leader_election, None);
    }

    // --- KOPIUR_MAX_CONCURRENT_DELETE_JOBS: `0`/unset ⇒ uncapped (the
    // default) — batching is the primary protection, this is an opt-in
    // backstop, so unlike worker-threads (clamp-to-1) a zero value is valid
    // and means "no cap" rather than being rejected. ---

    #[test]
    #[serial]
    fn max_concurrent_delete_jobs_zero_means_uncapped() {
        assert_eq!(
            resolve(&["--max-concurrent-delete-jobs", "0"]).max_concurrent_delete_jobs,
            None
        );
    }

    #[test]
    #[serial]
    fn max_concurrent_delete_jobs_garbage_fails_loudly() {
        let err = ControllerArgs::try_parse_from([
            "kopiur-controller",
            "--max-concurrent-delete-jobs",
            "many",
        ])
        .expect_err("garbage job count must not silently default");
        let msg = err.to_string();
        assert!(
            msg.contains("KOPIUR_MAX_CONCURRENT_DELETE_JOBS='many'"),
            "{msg}"
        );
        assert!(msg.contains("0 = uncapped"), "{msg}");
    }

    #[test]
    #[serial]
    fn max_concurrent_delete_jobs_flag_overrides_the_default() {
        assert_eq!(
            resolve(&["--max-concurrent-delete-jobs", "10"]).max_concurrent_delete_jobs,
            std::num::NonZeroUsize::new(10)
        );
    }

    // --- KOPIUR_MAX_CONCURRENT_JOBS: the cluster-wide POOL backstop under each
    // repository's own spec.concurrency.maxConcurrentJobs. Same `0`-means-
    // uncapped idiom as the delete-job cap and as the CRD field it backstops. ---

    #[test]
    #[serial]
    fn max_concurrent_jobs_defaults_to_uncapped() {
        // The default install must be byte-identical to pre-feature behavior:
        // `None` here is what makes the gate skip its Job LIST entirely.
        assert_eq!(resolve(&[]).max_concurrent_jobs, None);
    }

    #[test]
    #[serial]
    fn max_concurrent_jobs_zero_means_uncapped() {
        assert_eq!(
            resolve(&["--max-concurrent-jobs", "0"]).max_concurrent_jobs,
            None
        );
    }

    #[test]
    #[serial]
    fn max_concurrent_jobs_garbage_fails_loudly() {
        // A typo'd backstop must not silently become "no backstop": the
        // operator would believe a cap is in place that is not.
        let err =
            ControllerArgs::try_parse_from(["kopiur-controller", "--max-concurrent-jobs", "lots"])
                .expect_err("garbage job count must not silently default");
        let msg = err.to_string();
        assert!(msg.contains("KOPIUR_MAX_CONCURRENT_JOBS='lots'"), "{msg}");
        assert!(msg.contains("0 = uncapped"), "{msg}");
        // The message must point at the primary knob, not just itself.
        assert!(
            msg.contains("spec.concurrency.maxConcurrentJobs"),
            "the error must name the per-repository field it backstops: {msg}"
        );
    }

    #[test]
    #[serial]
    fn max_concurrent_jobs_flag_overrides_the_default() {
        assert_eq!(
            resolve(&["--max-concurrent-jobs", "4"]).max_concurrent_jobs,
            std::num::NonZeroUsize::new(4)
        );
    }

    #[test]
    #[serial]
    fn max_concurrent_jobs_is_distinct_from_the_delete_job_cap() {
        // Two different pools, two different env vars — a rename that collapsed
        // them would silently throttle backups with a delete cap (or vice versa).
        let cfg = resolve(&[
            "--max-concurrent-jobs",
            "2",
            "--max-concurrent-delete-jobs",
            "7",
        ]);
        assert_eq!(cfg.max_concurrent_jobs, std::num::NonZeroUsize::new(2));
        assert_eq!(
            cfg.max_concurrent_delete_jobs,
            std::num::NonZeroUsize::new(7)
        );
        assert_ne!(MAX_CONCURRENT_JOBS_ENV, MAX_CONCURRENT_DELETE_JOBS_ENV);
    }

    // --- kube client timeouts (apiserver-outage incident): the defaults were
    // read_timeout=None (a stalled established connection holds its fd until
    // TCP gives up) and connect_timeout=30s (each blackholed SYN pins an fd
    // for 30s — the dominant per-fd hold during the outage). These pin the
    // contract; the wiring is compile-checked in startup. ---

    #[test]
    #[serial]
    fn kube_client_timeouts_cover_the_watch_long_poll() {
        // kube-runtime requests 290s server-side watch timeouts and applies its
        // own 290+5s client idle timeout; a read timeout at or below that
        // effective 295s window would kill every healthy-but-quiet watch
        // stream mid-cycle.
        assert!(KUBE_CLIENT_READ_TIMEOUT > std::time::Duration::from_secs(295));
        // In-cluster the apiserver is one ClusterIP hop (connects are
        // ms-scale); anything close to the old 30s default re-opens the
        // blackholed-connect fd hold.
        assert!(KUBE_CLIENT_CONNECT_TIMEOUT <= std::time::Duration::from_secs(10));
    }

    // --- KOPIUR_RECONCILE_CONCURRENCY: BOUNDED by default (unlike the delete-Job
    // cap, where batching is the primary protection and uncapped is safe). kube's
    // implicit unbounded reconcile concurrency is what let a post-outage re-list
    // dispatch every object at once and exhaust the fd table (EMFILE) while the
    // apiserver flapped; `0` is the explicit unbounded escape hatch. ---

    #[test]
    fn default_lease_timings_are_valid() {
        LeaseTimings::default()
            .validate()
            .expect("the shipped defaults must satisfy the no-split-brain margin");
    }

    #[test]
    fn lease_timings_reject_a_set_that_could_split_brain() {
        use std::time::Duration;
        let secs = Duration::from_secs;
        // The #319 shape exactly: renewPeriod + renewDeadline == leaseDuration,
        // i.e. ZERO margin — a leader can still be reconciling at the instant a
        // standby becomes entitled to claim. (These were the shipped constants.)
        let zero_margin = LeaseTimings {
            lease_duration: secs(15),
            renew_deadline: secs(10),
            renew_period: secs(5),
            retry_period: secs(2),
        };
        let err = zero_margin
            .validate()
            .expect_err("zero margin must be rejected");
        assert!(
            err.to_string().contains("strictly less than"),
            "the error must explain the margin: {err}"
        );

        // A window that fits only one attempt is the other half of #319: one
        // slow call ends leadership with no retry.
        assert!(
            LeaseTimings {
                retry_period: secs(10),
                ..LeaseTimings::default()
            }
            .validate()
            .is_err()
        );

        // A zero cadence would busy-loop the API server.
        assert!(
            LeaseTimings {
                renew_period: Duration::ZERO,
                ..LeaseTimings::default()
            }
            .validate()
            .is_err()
        );

        // An absurd lease duration must be rejected on MAGNITUDE, not just on
        // its relationships. `leaseDurationSeconds` is an i32 on the wire, so a
        // value past that wraps silently and the Lease would advertise expiry
        // semantics this process does not enforce — to every other client
        // reading it, not just to us.
        let oversized = LeaseTimings {
            lease_duration: Duration::from_secs(u32::MAX as u64),
            renew_deadline: Duration::from_secs(u32::MAX as u64 - 100),
            renew_period: secs(2),
            retry_period: secs(2),
        };
        // Relationally consistent — the ONLY thing rejecting it is the ceiling.
        assert!(oversized.renew_period + oversized.renew_deadline < oversized.lease_duration);
        let err = oversized
            .validate()
            .expect_err("an i32-overflowing lease duration must be rejected");
        assert!(err.to_string().contains("ceiling"), "{err}");

        assert!(
            LeaseTimings {
                lease_duration: MAX_LEASE_DURATION,
                ..LeaseTimings::default()
            }
            .validate()
            .is_ok(),
            "the ceiling itself must be allowed"
        );
    }

    #[test]
    #[serial]
    fn lease_timings_are_env_configurable_and_validated_at_startup() {
        use std::time::Duration;
        let timings = resolve(&[
            "--leader-elect",
            "--operator-namespace",
            "kopiur-system",
            "--lease-duration-seconds",
            "30",
            "--renew-deadline-seconds",
            "20",
            "--renew-period-seconds",
            "5",
            "--retry-period-seconds",
            "3",
        ])
        .leader_election
        .expect("leader election is on")
        .timings;
        assert_eq!(timings.lease_duration, Duration::from_secs(30));
        assert_eq!(timings.renew_deadline, Duration::from_secs(20));
        assert_eq!(timings.renew_period, Duration::from_secs(5));
        assert_eq!(timings.retry_period, Duration::from_secs(3));

        // An invalid set must fail at STARTUP, not at the first stalled renew.
        let err = parse(&[
            "--leader-elect",
            "--operator-namespace",
            "kopiur-system",
            "--lease-duration-seconds",
            "10",
            "--renew-deadline-seconds",
            "10",
        ])
        .resolve()
        .expect_err("renewDeadline >= leaseDuration must not start");
        assert!(matches!(err, ConfigError::LeaseTimings(_)), "{err}");
    }

    #[test]
    #[serial]
    fn reconcile_concurrency_defaults_bounded() {
        // The bounded default IS the incident fix: unset must never mean
        // unbounded again.
        assert_eq!(
            resolve(&[]).reconcile_concurrency,
            std::num::NonZeroU16::new(DEFAULT_RECONCILE_CONCURRENCY)
        );
    }

    #[test]
    #[serial]
    fn reconcile_concurrency_zero_means_unbounded() {
        assert_eq!(
            resolve(&["--reconcile-concurrency", "0"]).reconcile_concurrency,
            None
        );
    }

    #[test]
    #[serial]
    fn reconcile_concurrency_empty_means_default_not_unbounded() {
        // The chart can render the env as ""; that has always meant "unset" —
        // which here must fall back to the BOUNDED default, not to 0/unbounded.
        assert_eq!(
            resolve(&["--reconcile-concurrency="]).reconcile_concurrency,
            std::num::NonZeroU16::new(DEFAULT_RECONCILE_CONCURRENCY)
        );
    }

    #[test]
    #[serial]
    fn reconcile_concurrency_garbage_fails_loudly() {
        let err = ControllerArgs::try_parse_from([
            "kopiur-controller",
            "--reconcile-concurrency",
            "many",
        ])
        .expect_err("garbage reconcile cap must not silently default");
        let msg = err.to_string();
        assert!(msg.contains("KOPIUR_RECONCILE_CONCURRENCY='many'"), "{msg}");
        assert!(msg.contains("0 = unbounded"), "{msg}");
        assert!(msg.contains("unset it to use the default"), "{msg}");
    }

    #[test]
    #[serial]
    fn reconcile_concurrency_flag_overrides_the_default() {
        assert_eq!(
            resolve(&["--reconcile-concurrency", "16"]).reconcile_concurrency,
            std::num::NonZeroU16::new(16)
        );
    }

    #[test]
    #[serial]
    fn worker_threads_zero_is_clamped_to_one() {
        // tokio's runtime builder panics on 0 worker threads; the documented
        // contract is clamp-to-1, not crash.
        assert_eq!(resolve(&["--worker-threads", "0"]).worker_threads, 1);
    }

    #[test]
    #[serial]
    fn worker_threads_garbage_fails_loudly() {
        // Previously a garbage KOPIUR_WORKER_THREADS silently fell back to the
        // default; the clap surface makes it a startup error instead.
        let err = ControllerArgs::try_parse_from(["kopiur-controller", "--worker-threads", "two"])
            .expect_err("garbage worker-thread count must not silently default");
        assert!(err.to_string().contains("--worker-threads"), "{err}");
    }

    #[test]
    #[serial]
    fn http_addr_invalid_value_fails_loudly_with_an_actionable_message() {
        let err =
            ControllerArgs::try_parse_from(["kopiur-controller", "--http-addr", "not-an-addr"])
                .expect_err("garbage KOPIUR_HTTP_ADDR must not silently fall back");
        let msg = err.to_string();
        // What: which var, what value. Why: not a valid socket address. Fix:
        // both accepted forms plus how to get back to the default.
        assert!(msg.contains("KOPIUR_HTTP_ADDR='not-an-addr'"), "{msg}");
        assert!(msg.contains("is not a valid socket address"), "{msg}");
        assert!(msg.contains("0.0.0.0:8081"), "{msg}");
        assert!(msg.contains("[::]:8081"), "{msg}");
        assert!(msg.contains("unset it to use the default"), "{msg}");
    }

    #[test]
    #[serial]
    fn bools_reject_garbage_instead_of_meaning_false() {
        // Previously any non-truthy KOPIUR_STREAMING_LISTS silently meant
        // "off"; a typo'd value now fails at startup instead of masking the
        // operator's intent.
        ControllerArgs::try_parse_from(["kopiur-controller", "--streaming-lists", "yolo"])
            .expect_err("garbage bool must not silently mean false");
        // The chart renders "true"/"false"; both (and bare-flag form) parse.
        assert!(resolve(&["--streaming-lists"]).streaming_lists);
        assert!(resolve(&["--streaming-lists", "true"]).streaming_lists);
        assert!(!resolve(&["--streaming-lists", "false"]).streaming_lists);
        assert!(resolve(&["--streaming-lists", "1"]).streaming_lists);
    }

    // --- env fallback: one smoke test per plumbing direction. Exhaustive
    // per-field env tests would re-test clap itself; the field↔env wiring is a
    // single `env = CONST` attribute per field, and the consts are asserted by
    // name here. `#[serial]` + unsafe set_var is the repo's env-test idiom
    // (process-global state; Rust runs tests concurrently by default). ---

    #[test]
    #[serial_test::serial]
    fn env_value_is_used_when_flag_is_absent() {
        // SAFETY: serialized by #[serial] against every other env-touching test.
        unsafe { std::env::set_var(HTTP_ADDR_ENV, "[::]:8081") };
        let cfg = resolve(&[]);
        unsafe { std::env::remove_var(HTTP_ADDR_ENV) };
        assert_eq!(cfg.http_addr, "[::]:8081".parse().unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn flag_beats_env() {
        // SAFETY: serialized by #[serial] against every other env-touching test.
        unsafe { std::env::set_var(HTTP_ADDR_ENV, "[::]:8081") };
        let cfg = resolve(&["--http-addr", "127.0.0.1:9999"]);
        unsafe { std::env::remove_var(HTTP_ADDR_ENV) };
        assert_eq!(cfg.http_addr, "127.0.0.1:9999".parse().unwrap());
    }

    // clap derive self-check: catches attribute mistakes (conflicting ids,
    // bad defaults) that only surface at runtime otherwise.
    #[test]
    #[serial]
    fn clap_debug_assert() {
        use clap::CommandFactory as _;
        ControllerArgs::command().debug_assert();
    }
}
