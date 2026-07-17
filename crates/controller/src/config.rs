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
/// Defaults to [`DEFAULT_MAX_CONCURRENT_DELETE_JOBS`]. Reachable via the
/// chart's `controller.maxConcurrentDeleteJobs`.
pub const MAX_CONCURRENT_DELETE_JOBS_ENV: &str = "KOPIUR_MAX_CONCURRENT_DELETE_JOBS";

/// Fallback cap when [`MAX_CONCURRENT_DELETE_JOBS_ENV`] is unset: 2 concurrent
/// batch-delete Jobs cluster-wide — enough to make progress on a queued wave
/// without saturating the backend.
pub const DEFAULT_MAX_CONCURRENT_DELETE_JOBS: usize = 2;

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
    /// every repository. `0` is rejected: it would wedge every deletion, not
    /// just a mass-deletion wave — use `deletionProtection.threshold: 0` (or
    /// the per-Snapshot skip-snapshot-cleanup annotation) to disable
    /// protection instead of trying to disable deletion via this knob.
    #[arg(long, env = MAX_CONCURRENT_DELETE_JOBS_ENV,
          default_value_t = DEFAULT_MAX_CONCURRENT_DELETE_JOBS,
          value_parser = parse_max_concurrent_delete_jobs)]
    pub max_concurrent_delete_jobs: usize,

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
    /// every repository (>= 1; `0` is rejected at parse time).
    pub max_concurrent_delete_jobs: usize,
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
            Some(LeaderElection {
                lease_name: nonempty(self.lease_name)
                    .unwrap_or_else(|| DEFAULT_LEASE_NAME.to_string()),
                namespace: operator_namespace
                    .clone()
                    .ok_or(ConfigError::LeaderElectionNeedsNamespace)?,
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
            max_concurrent_delete_jobs: self.max_concurrent_delete_jobs,
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
/// the default); `0` is REJECTED — a zero cap would wedge every Snapshot
/// deletion cluster-wide, not just a mass-deletion wave, which is never the
/// intended remedy (the message names the actual knobs for that intent).
fn parse_max_concurrent_delete_jobs(value: &str) -> Result<usize, String> {
    if value.is_empty() {
        return Ok(DEFAULT_MAX_CONCURRENT_DELETE_JOBS);
    }
    match value.parse::<usize>() {
        Ok(0) => Err(format!(
            "KOPIUR_MAX_CONCURRENT_DELETE_JOBS='0' would wedge every Snapshot deletion \
             cluster-wide, not just a mass-deletion wave; to disable the mass-deletion breaker \
             instead, set deletionProtection.threshold: 0 on the repository (or use the \
             per-Snapshot skip-snapshot-cleanup annotation); unset this to use the default \
             {DEFAULT_MAX_CONCURRENT_DELETE_JOBS}"
        )),
        Ok(n) => Ok(n),
        Err(_) => Err(format!(
            "KOPIUR_MAX_CONCURRENT_DELETE_JOBS='{value}' is not a valid job count; use a \
             positive integer, e.g. 2; unset it to use the default \
             {DEFAULT_MAX_CONCURRENT_DELETE_JOBS}"
        )),
    }
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
        assert_eq!(
            cfg.max_concurrent_delete_jobs,
            DEFAULT_MAX_CONCURRENT_DELETE_JOBS
        );
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
        assert_eq!(cfg.max_concurrent_delete_jobs, 4);
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
        assert_eq!(
            cfg.max_concurrent_delete_jobs,
            DEFAULT_MAX_CONCURRENT_DELETE_JOBS
        );
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

    // --- KOPIUR_MAX_CONCURRENT_DELETE_JOBS: 0 is a hard reject (would wedge
    // every deletion), unlike worker-threads' clamp-to-1. ---

    #[test]
    #[serial]
    fn max_concurrent_delete_jobs_zero_is_rejected_with_an_actionable_message() {
        // Like worker-threads/sweep-interval, the value_parser runs at PARSE
        // time (not resolve()), so the rejection surfaces from try_parse_from.
        let err = ControllerArgs::try_parse_from([
            "kopiur-controller",
            "--max-concurrent-delete-jobs",
            "0",
        ])
        .expect_err("a zero cap must not silently wedge every deletion");
        let msg = err.to_string();
        assert!(
            msg.contains("KOPIUR_MAX_CONCURRENT_DELETE_JOBS='0'"),
            "{msg}"
        );
        assert!(msg.contains("deletionProtection.threshold: 0"), "{msg}");
        assert!(msg.contains("skip-snapshot-cleanup"), "{msg}");
        assert!(msg.contains("unset this to use the default"), "{msg}");
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
        assert!(
            err.to_string()
                .contains("KOPIUR_MAX_CONCURRENT_DELETE_JOBS='many'"),
            "{err}"
        );
    }

    #[test]
    #[serial]
    fn max_concurrent_delete_jobs_flag_overrides_the_default() {
        assert_eq!(
            resolve(&["--max-concurrent-delete-jobs", "10"]).max_concurrent_delete_jobs,
            10
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
