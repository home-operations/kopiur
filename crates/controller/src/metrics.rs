//! Controller metrics (ADR §4.13 / §4.10).
//!
//! Instrumented **once** against the OpenTelemetry metrics API and fanned out to
//! two readers by [`kopiur_telemetry::MetricsProvider`]: an always-on Prometheus
//! exporter (the `/metrics` pull endpoint + `ServiceMonitor`) and — when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set — an OTLP push reader. Recording a value
//! updates both; there is no double instrumentation.
//!
//! Every metric is under the `kopiur_` namespace. The Prometheus exporter
//! applies the usual OTel→Prometheus conventions, so a `u64_counter` named
//! `kopiur_controller_reconciliations` is exported as
//! `kopiur_controller_reconciliations_total`. The `/metrics` text is rendered by
//! [`Metrics::gather`]; the HTTP server lives in `lib.rs`.
//!
//! [`Metrics`] is cloned into the shared [`crate::context::Context`]; the
//! OpenTelemetry instruments and the provider are internally reference-counted,
//! so clones share state.

use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram};

use kube::ResourceExt;
use kube::runtime::reflector::Store;

use kopiur_api::{
    ClusterRepository, PhaseLabel, Repository, Restore, Snapshot, SnapshotPhase, SnapshotStats,
};
use kopiur_telemetry::MetricsProvider;

/// Resident set size (RSS) of the current process in bytes, read from Linux
/// `/proc/self/statm` (field 2 is the resident page count). Returns `None` off
/// Linux or on any read/parse failure, so the gauge is simply absent rather than
/// fabricated — telemetry is non-critical.
fn resident_memory_bytes() -> Option<i64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: i64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: `sysconf` is a pure lookup with no preconditions or side effects.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (page_size > 0).then(|| resident_pages.saturating_mul(page_size))
}

/// All controller metrics, sharing one meter provider + Prometheus registry.
#[derive(Clone)]
pub struct Metrics {
    provider: Arc<MetricsProvider>,

    // Reconcile loop (kube-rs standard).
    reconciliations: Counter<u64>,
    reconcile_errors: Counter<u64>,
    reconcile_duration: Histogram<f64>,

    // Snapshot business metrics.
    //
    // Per-resource phase (`kopiur_resource_phase`) and the per-Snapshot
    // size/files/duration/last-success gauges are NOT held here: they are
    // store-backed i64 *observable* gauges registered in
    // [`Metrics::register_resource_observers`], whose callbacks enumerate the
    // controllers' reflector stores at collection time. A series exists iff its CR
    // does — the callback is the sole source of truth per collection cycle, so an
    // attribute set it doesn't observe is simply absent from that cycle's point
    // set, and a deleted CR's series genuinely disappears from `/metrics` (the
    // #172/#175 fix; a sync gauge could only zero a series, never remove it).
    backup_verified_timestamp: Gauge<i64>,
    backup_consecutive_failures: Gauge<i64>,
    snapshots_completed: Counter<u64>,
    snapshot_deletion_failures: Counter<u64>,
    orphaned_snapshots: Counter<u64>,
    schedule_backups_created: Counter<u64>,
    secrets_projected: Counter<u64>,
    backups_refused: Counter<u64>,
    health_probe_failures: Counter<u64>,

    // Repository business metrics.
    repo_size_bytes: Gauge<i64>,
    repo_snapshot_count: Gauge<i64>,
    repo_discovered_backups: Gauge<i64>,
    repo_maintenance_configured: Gauge<i64>,

    // Restore + maintenance.
    restore_duration_seconds: Gauge<i64>,
    maintenance_reclaimed_bytes: Gauge<i64>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Build the meter provider (Prometheus + optional OTLP) and every
    /// instrument. Infallible: a provider build failure degrades to an empty
    /// `/metrics` rather than crashing the controller (telemetry is non-critical).
    pub fn new() -> Self {
        let provider = MetricsProvider::new("kopiur-controller");
        let m = provider.meter();

        let reconciliations = m
            .u64_counter("kopiur_controller_reconciliations")
            .with_description("Total reconciliations per CRD kind.")
            .build();
        let reconcile_errors = m
            .u64_counter("kopiur_controller_reconcile_errors")
            .with_description("Total reconcile errors per CRD kind and error class.")
            .build();
        let reconcile_duration = m
            .f64_histogram("kopiur_controller_reconcile_duration_seconds")
            .with_description("Reconcile duration in seconds per CRD kind.")
            .with_boundaries(vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0])
            .build();

        // Process RSS, sampled at scrape time. The returned handle is a phantom
        // marker — the callback is retained by the meter provider — so it is built
        // and discarded. Surfaces the controller's footprint on `/metrics` and guards
        // the memory-reduction work (mimalloc, worker-thread cap, scoped/metadata
        // watches) against regressions. Bytes; absent off Linux.
        // Bytes are in the name (matching kopiur_snapshot_size_bytes etc.) with no
        // `with_unit` — the Prometheus exporter appends a unit suffix, which would
        // otherwise produce a doubled `..._bytes_bytes`.
        let _ = m
            .i64_observable_gauge("kopiur_process_resident_memory_bytes")
            .with_description("Resident set size (RSS) of the controller process, in bytes.")
            .with_callback(|observer| {
                if let Some(rss) = resident_memory_bytes() {
                    observer.observe(rss, &[]);
                }
            })
            .build();

        let backup_verified_timestamp = m
            .i64_gauge("kopiur_snapshot_verified_timestamp_seconds")
            .with_description(
                "Unix timestamp of the most recent successful SnapshotPolicy verification \
                 (quick or deep), from status.lastVerified.",
            )
            .build();
        let backup_consecutive_failures = m
            .i64_gauge("kopiur_snapshot_consecutive_failures")
            .with_description("Number of consecutive backup failures.")
            .build();
        let snapshots_completed = m
            .u64_counter("kopiur_snapshots_completed")
            .with_description(
                "Total Snapshot runs that reached a terminal phase, by result \
                 (succeeded|failed), namespace, and policy. Incremented once per terminal \
                 transition (the controller's finalize paths), so unlike the observable \
                 kopiur_resource_phase gauge it survives the CR's deletion and answers \
                 time-windowed 'backups completed in this period' in PromQL (issue #175).",
            )
            .build();
        let snapshot_deletion_failures = m
            .u64_counter("kopiur_snapshot_deletion_failures")
            .with_description("Total kopia snapshot-deletion failures during finalizer handling.")
            .build();
        let orphaned_snapshots = m
            .u64_counter("kopiur_orphaned_snapshots")
            .with_description(
                "Total snapshots orphaned (Orphan policy or skip-snapshot-cleanup annotation).",
            )
            .build();
        let schedule_backups_created = m
            .u64_counter("kopiur_schedule_snapshots_created")
            .with_description("Total Snapshot CRs created by a SnapshotSchedule.")
            .build();
        let secrets_projected = m
            .u64_counter("kopiur_secrets_projected")
            .with_description(
                "Total credential Secrets projected into a mover Job's namespace \
                 (opt-in spec.credentialProjection).",
            )
            .build();
        let backups_refused = m
            .u64_counter("kopiur_snapshot_refusals")
            .with_description(
                "Total backups refused by policy (e.g. a ReadOnly repository, a privileged \
                 mover without the namespace opt-in), labeled by reason. Refusals are \
                 deliberate decisions, not reconcile errors, so they are not in \
                 kopiur_controller_reconcile_errors.",
            )
            .build();
        let health_probe_failures = m
            .u64_counter("kopiur_repository_health_probe_failures")
            .with_description(
                "Total backend health-probe alerts raised (after the consecutive-failure \
                 debounce), labeled by kind and outcome (vanished = backend reachable but the \
                 repository is absent; unreachable = backend/mount/auth failure). The repository \
                 stays Ready — these are alerts, not outages, and kopiur never auto-recreates.",
            )
            .build();

        let repo_size_bytes = m
            .i64_gauge("kopiur_repo_size_bytes")
            .with_description(
                "Logical bytes under management (sum of the latest snapshot per source).",
            )
            .build();
        let repo_snapshot_count = m
            .i64_gauge("kopiur_repo_snapshot_count")
            .with_description("Number of snapshots in the repository.")
            .build();
        let repo_discovered_backups = m
            .i64_gauge("kopiur_repo_discovered_snapshots")
            .with_description("Number of backups discovered in the repository catalog.")
            .build();
        let repo_maintenance_configured = m
            .i64_gauge("kopiur_repository_maintenance_configured")
            .with_description(
                "1 if a Maintenance CR references the repository, 0 otherwise (unmaintained \
                 repositories never reclaim storage).",
            )
            .build();

        let restore_duration_seconds = m
            .i64_gauge("kopiur_restore_duration_seconds")
            .with_description("Wall-clock duration in seconds of the last restore Job.")
            .build();
        let maintenance_reclaimed_bytes = m
            .i64_gauge("kopiur_maintenance_last_reclaimed_bytes")
            .with_description("Bytes reclaimed by the last full maintenance run.")
            .build();

        Metrics {
            provider: Arc::new(provider),
            reconciliations,
            reconcile_errors,
            reconcile_duration,
            backup_verified_timestamp,
            backup_consecutive_failures,
            snapshots_completed,
            snapshot_deletion_failures,
            orphaned_snapshots,
            schedule_backups_created,
            secrets_projected,
            backups_refused,
            health_probe_failures,
            repo_size_bytes,
            repo_snapshot_count,
            repo_discovered_backups,
            repo_maintenance_configured,
            restore_duration_seconds,
            maintenance_reclaimed_bytes,
        }
    }

    // ---- reconcile loop ----------------------------------------------------

    /// Record a completed reconcile of `kind` lasting `seconds`.
    pub fn record_reconcile(&self, kind: &str, seconds: f64) {
        let attrs = [KeyValue::new("kind", kind.to_string())];
        self.reconciliations.add(1, &attrs);
        self.reconcile_duration.record(seconds, &attrs);
    }

    /// Record a reconcile error of `kind` with the given error `class`.
    pub fn record_error(&self, kind: &str, class: &str) {
        self.reconcile_errors.add(
            1,
            &[
                KeyValue::new("kind", kind.to_string()),
                KeyValue::new("class", class.to_string()),
            ],
        );
    }

    // ---- store-backed observable gauges ------------------------------------

    /// Register the observable gauges whose callbacks enumerate the controllers'
    /// reflector `Store`s at collection time. Call ONCE from `spawn_all` after the
    /// store handles exist (before the controllers are joined).
    ///
    /// Why observable (not sync): a sync gauge series can only be overwritten, never
    /// dropped, so a deleted or GC'd CR's `kopiur_resource_phase{...}==1` /
    /// `kopiur_snapshot_*` series would linger forever and keep firing staleness
    /// alerts (#172/#175). An observable gauge is re-derived from live store state
    /// each cycle, and the callback is the sole source of truth for that cycle: an
    /// attribute set it doesn't emit is simply absent from the collected point set,
    /// so a series disappears from the next Prometheus exposition — a series exists
    /// **iff** its CR exists.
    ///
    /// `kopiur_resource_phase` emits `1` for the **active** phase only — never a
    /// 0-valued series for the inactive phases (the old enumerate-and-reset flooded
    /// `/metrics` with `phase="…"}=0` lines). The active phase is `status.phase`, or
    /// the phase enum's `Default` when status is absent, so a just-created CR is
    /// visible (`Pending`/`Initializing`) rather than missing.
    ///
    /// The `policy` label (Snapshot series only) is **omitted** for snapshots with no
    /// `spec.policyRef` (discovered snapshots): the Prometheus exporter renders a
    /// missing attribute as no label at all, which is cleaner than `policy=""` and
    /// keeps discovered snapshots out of PromQL `by (policy)` groupings.
    pub fn register_resource_observers(&self, stores: ResourceStores) {
        let m = self.provider.meter();

        // Per-resource lifecycle phase across all four store-backed kinds. One series
        // per CR, valued 1 at its active phase.
        {
            let snapshots = stores.snapshots.clone();
            let repositories = stores.repositories.clone();
            let cluster_repositories = stores.cluster_repositories.clone();
            let restores = stores.restores.clone();
            let _ = m
                .i64_observable_gauge("kopiur_resource_phase")
                .with_description(
                    "1 for a resource's active lifecycle phase (labeled kind/namespace/name/phase, \
                     plus policy on Snapshot series). Store-backed: the series exists only while \
                     the CR does, and only the active phase is emitted — never a 0-valued series.",
                )
                .with_callback(move |o| {
                    for s in snapshots.state() {
                        let phase = s
                            .status
                            .as_ref()
                            .and_then(|st| st.phase)
                            .unwrap_or_default();
                        let mut attrs = vec![
                            KeyValue::new("kind", "Snapshot"),
                            KeyValue::new("namespace", s.namespace().unwrap_or_default()),
                            KeyValue::new("name", s.name_any()),
                            KeyValue::new("phase", phase.label()),
                        ];
                        if let Some(pr) = s.spec.policy_ref.as_ref() {
                            attrs.push(KeyValue::new("policy", pr.name.clone()));
                        }
                        o.observe(1, &attrs);
                    }
                    for r in repositories.state() {
                        let phase = r
                            .status
                            .as_ref()
                            .and_then(|st| st.phase)
                            .unwrap_or_default();
                        o.observe(
                            1,
                            &phase_attrs(
                                "Repository",
                                &r.namespace().unwrap_or_default(),
                                &r.name_any(),
                                phase.label(),
                            ),
                        );
                    }
                    for r in cluster_repositories.state() {
                        let phase = r
                            .status
                            .as_ref()
                            .and_then(|st| st.phase)
                            .unwrap_or_default();
                        // Cluster-scoped: empty namespace, matching the sync convention.
                        o.observe(
                            1,
                            &phase_attrs("ClusterRepository", "", &r.name_any(), phase.label()),
                        );
                    }
                    for r in restores.state() {
                        let phase = r
                            .status
                            .as_ref()
                            .and_then(|st| st.phase)
                            .unwrap_or_default();
                        o.observe(
                            1,
                            &phase_attrs(
                                "Restore",
                                &r.namespace().unwrap_or_default(),
                                &r.name_any(),
                                phase.label(),
                            ),
                        );
                    }
                })
                .build();
        }

        // Per-Snapshot logical size in bytes, only when recorded.
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_snapshot_size_bytes")
                .with_description(
                    "Logical size in bytes of a Snapshot, from status.stats.sizeBytes.",
                )
                .with_callback(move |o| {
                    for s in snapshots.state() {
                        if let Some(size) = s
                            .status
                            .as_ref()
                            .and_then(|st| st.stats.as_ref())
                            .and_then(|st| st.size_bytes)
                        {
                            o.observe(size, &snapshot_stat_attrs(&s));
                        }
                    }
                })
                .build();
        }

        // Per-Snapshot file count, only when known (unknown != 0).
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_snapshot_files")
                .with_description(
                    "File count of a Snapshot (new+modified+unchanged), only when at least one \
                     category is recorded — an unmeasured count is absent, never a bogus 0.",
                )
                .with_callback(move |o| {
                    for s in snapshots.state() {
                        if let Some(files) = s
                            .status
                            .as_ref()
                            .and_then(|st| st.stats.as_ref())
                            .and_then(snapshot_file_count)
                        {
                            o.observe(files, &snapshot_stat_attrs(&s));
                        }
                    }
                })
                .build();
        }

        // Per-Snapshot duration in seconds, only when recorded.
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_snapshot_duration_seconds")
                .with_description(
                    "Duration in seconds of a Snapshot, from status.timing.durationSeconds.",
                )
                .with_callback(move |o| {
                    for s in snapshots.state() {
                        if let Some(dur) = s
                            .status
                            .as_ref()
                            .and_then(|st| st.timing.as_ref())
                            .and_then(|t| t.duration_seconds)
                        {
                            o.observe(dur, &snapshot_stat_attrs(&s));
                        }
                    }
                })
                .build();
        }

        // Per-Snapshot last-success timestamp: the MOVER-recorded status.timing.endTime
        // (a semantic change from the old Utc::now()-at-reconcile stamp), emitted only
        // for a Succeeded Snapshot. Feeds the Helm staleness alert.
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_snapshot_last_success_timestamp_seconds")
                .with_description(
                    "Unix timestamp of a successful Snapshot, from the mover-recorded \
                     status.timing.endTime (only while phase == Succeeded).",
                )
                .with_callback(move |o| {
                    for s in snapshots.state() {
                        if let Some(ts) = snapshot_success_unix(&s) {
                            o.observe(ts, &snapshot_stat_attrs(&s));
                        }
                    }
                })
                .build();
        }

        // Per-policy "latest successful backup" family: answers "latest per policy",
        // which the per-CR series cannot in PromQL (`max by` returns the max value,
        // not the newest run — issue #172). Each metric reflects the Succeeded
        // Snapshot with the greatest endTime per (namespace, policy).
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_policy_last_backup_success_timestamp_seconds")
                .with_description(
                    "endTime (unix seconds) of the latest Succeeded Snapshot per (namespace, policy).",
                )
                .with_callback(move |o| {
                    for p in latest_per_policy(&snapshots.state()) {
                        o.observe(p.end_unix, &policy_attrs(&p));
                    }
                })
                .build();
        }
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_policy_last_backup_duration_seconds")
                .with_description(
                    "Duration in seconds of the latest Succeeded Snapshot per (namespace, policy).",
                )
                .with_callback(move |o| {
                    for p in latest_per_policy(&snapshots.state()) {
                        if let Some(v) = p.duration_seconds {
                            o.observe(v, &policy_attrs(&p));
                        }
                    }
                })
                .build();
        }
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_policy_last_backup_size_bytes")
                .with_description(
                    "Logical size in bytes of the latest Succeeded Snapshot per (namespace, policy).",
                )
                .with_callback(move |o| {
                    for p in latest_per_policy(&snapshots.state()) {
                        if let Some(v) = p.size_bytes {
                            o.observe(v, &policy_attrs(&p));
                        }
                    }
                })
                .build();
        }
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_policy_last_backup_files")
                .with_description(
                    "File count of the latest Succeeded Snapshot per (namespace, policy), only \
                     when the count is known.",
                )
                .with_callback(move |o| {
                    for p in latest_per_policy(&snapshots.state()) {
                        if let Some(v) = p.files {
                            o.observe(v, &policy_attrs(&p));
                        }
                    }
                })
                .build();
        }
    }

    // ---- backup business metrics -------------------------------------------

    /// Count a Snapshot reaching a terminal phase. `result` is `succeeded` or
    /// `failed`; `policy` is omitted for a Snapshot without a `policyRef`. Call this
    /// exactly once per terminal transition — at the controller's finalize paths, NOT
    /// on every reconcile of an already-terminal Snapshot (reconciles re-run). Unlike
    /// the observable phase gauge, this counter is durable: it answers a
    /// time-windowed "how many backups completed" (issue #175).
    pub fn inc_snapshot_completed(&self, result: &'static str, ns: &str, policy: Option<&str>) {
        let mut attrs = vec![
            KeyValue::new("result", result),
            KeyValue::new("namespace", ns.to_string()),
        ];
        if let Some(policy) = policy {
            attrs.push(KeyValue::new("policy", policy.to_string()));
        }
        self.snapshots_completed.add(1, &attrs);
    }

    /// Stamp the Unix timestamp of a SnapshotPolicy's most recent successful
    /// verification (from `status.lastVerified`) for staleness alerting (ADR-0005 §4):
    /// `time() - kopiur_snapshot_verified_timestamp_seconds` is the verify age.
    pub fn set_snapshot_verified(&self, ns: &str, name: &str, ts: i64) {
        self.backup_verified_timestamp
            .record(ts, &ns_name(ns, name));
    }

    /// Set the consecutive-failure count for a SnapshotPolicy.
    pub fn set_backup_consecutive_failures(&self, ns: &str, name: &str, n: i64) {
        self.backup_consecutive_failures
            .record(n, &ns_name(ns, name));
    }

    /// Count `n` credential Secrets projected into mover namespace `ns` (opt-in
    /// `spec.credentialProjection`).
    pub fn inc_secrets_projected(&self, ns: &str, n: u64) {
        self.secrets_projected
            .add(n, &[KeyValue::new("namespace", ns.to_string())]);
    }

    /// Count a snapshot-deletion (finalizer) failure in `namespace`.
    pub fn inc_snapshot_deletion_failure(&self, ns: &str) {
        self.snapshot_deletion_failures
            .add(1, &[KeyValue::new("namespace", ns.to_string())]);
    }

    /// Count a snapshot orphaned (Orphan policy / escape hatch) in `namespace`.
    pub fn inc_orphaned_snapshot(&self, ns: &str) {
        self.orphaned_snapshots
            .add(1, &[KeyValue::new("namespace", ns.to_string())]);
    }

    /// Count a Snapshot CR created by a SnapshotSchedule.
    pub fn inc_schedule_backup_created(&self, ns: &str, name: &str) {
        self.schedule_backups_created.add(1, &ns_name(ns, name));
    }

    /// Count a backup refused by policy. `reason` is the same machine-readable
    /// label as the Event/condition reason (e.g. `RepositoryReadOnly`,
    /// `PrivilegedMoverNotPermitted`) so dashboards and `kubectl get events`
    /// agree on the cause.
    pub fn inc_backup_refused(&self, ns: &str, name: &str, reason: &'static str) {
        self.backups_refused.add(
            1,
            &[
                KeyValue::new("namespace", ns.to_string()),
                KeyValue::new("name", name.to_string()),
                KeyValue::new("reason", reason),
            ],
        );
    }

    /// Count a backend health-probe alert (raised only after the debounce). `kind`
    /// is the repository kind (`Repository`/`ClusterRepository`); `outcome` is
    /// `vanished` or `unreachable`. Mirrors the `BackendReachable` condition reason.
    pub fn inc_health_probe_failure(&self, ns: &str, name: &str, kind: &str, outcome: &str) {
        self.health_probe_failures.add(
            1,
            &[
                KeyValue::new("namespace", ns.to_string()),
                KeyValue::new("name", name.to_string()),
                KeyValue::new("kind", kind.to_string()),
                KeyValue::new("outcome", outcome.to_string()),
            ],
        );
    }

    // ---- repository / restore / maintenance --------------------------------

    /// Set the repository size gauge.
    pub fn set_repo_size_bytes(&self, ns: &str, name: &str, bytes: i64) {
        self.repo_size_bytes.record(bytes, &ns_name(ns, name));
    }

    /// Set the repository snapshot-count and discovered-backup gauges.
    pub fn set_repo_catalog(
        &self,
        ns: &str,
        name: &str,
        snapshot_count: Option<i64>,
        discovered: Option<i64>,
    ) {
        let labels = ns_name(ns, name);
        if let Some(v) = snapshot_count {
            self.repo_snapshot_count.record(v, &labels);
        }
        if let Some(v) = discovered {
            self.repo_discovered_backups.record(v, &labels);
        }
    }

    /// Set the maintenance-configured gauge for a repository: 1 if a `Maintenance`
    /// CR references it, 0 otherwise. `kind` is `Repository`/`ClusterRepository`;
    /// `ns` is empty for a cluster-scoped `ClusterRepository`.
    pub fn set_repository_maintenance_configured(
        &self,
        kind: &str,
        ns: &str,
        name: &str,
        configured: bool,
    ) {
        self.repo_maintenance_configured.record(
            configured as i64,
            &[
                KeyValue::new("kind", kind.to_string()),
                KeyValue::new("namespace", ns.to_string()),
                KeyValue::new("name", name.to_string()),
            ],
        );
    }

    /// Set the last restore's duration gauge.
    pub fn set_restore_duration(&self, ns: &str, name: &str, seconds: i64) {
        self.restore_duration_seconds
            .record(seconds, &ns_name(ns, name));
    }

    /// Set the last full-maintenance reclaimed-bytes gauge.
    pub fn set_maintenance_reclaimed_bytes(&self, ns: &str, name: &str, bytes: i64) {
        self.maintenance_reclaimed_bytes
            .record(bytes, &ns_name(ns, name));
    }

    // ---- exposition --------------------------------------------------------

    /// Render the Prometheus text exposition for the `/metrics` endpoint.
    pub fn gather(&self) -> Vec<u8> {
        self.provider.gather()
    }
}

fn ns_name(ns: &str, name: &str) -> [KeyValue; 2] {
    [
        KeyValue::new("namespace", ns.to_string()),
        KeyValue::new("name", name.to_string()),
    ]
}

/// The reflector `Store` handles the observable-gauge callbacks read at collection
/// time. `Store<K>` is `Clone` (a shared cache handle); the callbacks clone the
/// ones they need.
pub struct ResourceStores {
    /// The `Snapshot` controller's reflector cache.
    pub snapshots: Store<Snapshot>,
    /// The `Repository` controller's reflector cache.
    pub repositories: Store<Repository>,
    /// The `ClusterRepository` controller's reflector cache.
    pub cluster_repositories: Store<ClusterRepository>,
    /// The `Restore` controller's reflector cache.
    pub restores: Store<Restore>,
}

/// Attributes for a `kopiur_resource_phase` series without a `policy` label
/// (Repository/ClusterRepository/Restore, and discovered Snapshots go through the
/// Snapshot-specific path).
fn phase_attrs(kind: &'static str, ns: &str, name: &str, phase: &'static str) -> [KeyValue; 4] {
    [
        KeyValue::new("kind", kind),
        KeyValue::new("namespace", ns.to_string()),
        KeyValue::new("name", name.to_string()),
        KeyValue::new("phase", phase),
    ]
}

/// Attributes for a per-Snapshot stat series: `namespace`, `name`, and `policy`
/// only when the Snapshot has a `policyRef` (omitted for discovered snapshots).
fn snapshot_stat_attrs(s: &Snapshot) -> Vec<KeyValue> {
    let mut attrs = vec![
        KeyValue::new("namespace", s.namespace().unwrap_or_default()),
        KeyValue::new("name", s.name_any()),
    ];
    if let Some(pr) = s.spec.policy_ref.as_ref() {
        attrs.push(KeyValue::new("policy", pr.name.clone()));
    }
    attrs
}

/// Attributes for a per-policy "latest" series: `namespace`, `policy`.
fn policy_attrs(p: &PolicyLatest) -> [KeyValue; 2] {
    [
        KeyValue::new("namespace", p.namespace.clone()),
        KeyValue::new("policy", p.policy.clone()),
    ]
}

/// File count for a Snapshot's stats, preserving the "unknown != 0" rule: `Some`
/// only when at least one of new/modified/unchanged is recorded, so an unmeasured
/// count is an absent series rather than a bogus `0`. Pure — unit-tested off-OTel.
fn snapshot_file_count(stats: &SnapshotStats) -> Option<i64> {
    match (stats.files_new, stats.files_modified, stats.files_unchanged) {
        (None, None, None) => None,
        (a, b, c) => Some(a.unwrap_or(0) + b.unwrap_or(0) + c.unwrap_or(0)),
    }
}

/// Parse an RFC3339 timestamp to whole unix seconds; `None` on a parse failure.
fn parse_unix_seconds(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.timestamp())
}

/// The mover-recorded success timestamp of a Snapshot: `status.timing.endTime` as
/// unix seconds, but only when `phase == Succeeded`. `None` otherwise.
fn snapshot_success_unix(s: &Snapshot) -> Option<i64> {
    let status = s.status.as_ref()?;
    if status.phase != Some(SnapshotPhase::Succeeded) {
        return None;
    }
    status
        .timing
        .as_ref()?
        .end_time
        .as_deref()
        .and_then(parse_unix_seconds)
}

/// The latest successful backup for one (namespace, policy) group.
#[derive(Debug, Clone, PartialEq)]
struct PolicyLatest {
    namespace: String,
    policy: String,
    /// `status.timing.endTime` (unix seconds) of the winning Snapshot.
    end_unix: i64,
    duration_seconds: Option<i64>,
    size_bytes: Option<i64>,
    files: Option<i64>,
}

/// Reduce a Snapshot set to, per (namespace, policy), the **Succeeded** Snapshot
/// with the greatest `status.timing.endTime`. Snapshots without a `policyRef`,
/// without a Succeeded phase, or without a parseable `endTime` never participate.
/// Pure so "latest per policy" (unanswerable in PromQL, issue #172) is unit-tested
/// off-cluster. Output is sorted by (namespace, policy) for deterministic tests.
fn latest_per_policy(snapshots: &[Arc<Snapshot>]) -> Vec<PolicyLatest> {
    use std::collections::HashMap;
    let mut best: HashMap<(String, String), PolicyLatest> = HashMap::new();
    for s in snapshots {
        let Some(policy) = s.spec.policy_ref.as_ref().map(|p| p.name.clone()) else {
            continue;
        };
        let Some(status) = s.status.as_ref() else {
            continue;
        };
        if status.phase != Some(SnapshotPhase::Succeeded) {
            continue;
        }
        let Some(end_unix) = status
            .timing
            .as_ref()
            .and_then(|t| t.end_time.as_deref())
            .and_then(parse_unix_seconds)
        else {
            continue;
        };
        let namespace = s.namespace().unwrap_or_default();
        let candidate = PolicyLatest {
            namespace: namespace.clone(),
            policy: policy.clone(),
            end_unix,
            duration_seconds: status.timing.as_ref().and_then(|t| t.duration_seconds),
            size_bytes: status.stats.as_ref().and_then(|st| st.size_bytes),
            files: status.stats.as_ref().and_then(snapshot_file_count),
        };
        best.entry((namespace, policy))
            .and_modify(|cur| {
                if candidate.end_unix > cur.end_unix {
                    *cur = candidate.clone();
                }
            })
            .or_insert(candidate);
    }
    let mut out: Vec<PolicyLatest> = best.into_values().collect();
    out.sort_by(|a, b| {
        (a.namespace.as_str(), a.policy.as_str()).cmp(&(b.namespace.as_str(), b.policy.as_str()))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::runtime::reflector::store::Writer;
    use kube::runtime::{reflector, watcher};
    use std::hash::Hash;

    // A succeeded backup happened on 2026-05-24T02:18:42Z = 1_779_589_122 unix.
    const END_TIME: &str = "2026-05-24T02:18:42Z";
    const END_UNIX: i64 = 1_779_589_122;

    fn make_store<K>(objs: Vec<K>) -> (Store<K>, Writer<K>)
    where
        K: reflector::Lookup + Clone + 'static,
        K::DynamicType: Eq + Hash + Clone + Default,
    {
        let (reader, mut writer) = reflector::store::<K>();
        for o in objs {
            writer.apply_watcher_event(&watcher::Event::Apply(o));
        }
        (reader, writer)
    }

    type StorePair<K> = (Store<K>, Writer<K>);

    fn empty_stores() -> (
        StorePair<Snapshot>,
        StorePair<Repository>,
        StorePair<ClusterRepository>,
        StorePair<Restore>,
    ) {
        (
            make_store(vec![]),
            make_store(vec![]),
            make_store(vec![]),
            make_store(vec![]),
        )
    }

    fn snapshot_cr(
        ns: &str,
        name: &str,
        policy: Option<&str>,
        status: serde_json::Value,
    ) -> Snapshot {
        let mut spec = serde_json::json!({});
        if let Some(p) = policy {
            spec["policyRef"] = serde_json::json!({ "name": p });
        }
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Snapshot",
            "metadata": { "name": name, "namespace": ns },
            "spec": spec,
            "status": status,
        }))
        .expect("valid Snapshot")
    }

    /// A Succeeded status with stats + timing.
    fn succeeded_status(size: i64, dur: i64, end_time: &str) -> serde_json::Value {
        serde_json::json!({
            "phase": "Succeeded",
            "timing": { "endTime": end_time, "durationSeconds": dur },
            "stats": { "sizeBytes": size, "filesNew": 3, "filesModified": 1, "filesUnchanged": 6 },
        })
    }

    fn repository_cr(ns: &str, name: &str, phase: &str) -> Repository {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Repository",
            "metadata": { "name": name, "namespace": ns },
            "spec": {
                "backend": { "filesystem": { "path": "/repo" } },
                "encryption": { "passwordSecretRef": { "name": "s" } },
            },
            "status": { "phase": phase },
        }))
        .expect("valid Repository")
    }

    fn cluster_repository_cr(name: &str, phase: &str) -> ClusterRepository {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "ClusterRepository",
            "metadata": { "name": name },
            "spec": {
                "backend": { "filesystem": { "path": "/repo" } },
                "encryption": { "passwordSecretRef": { "name": "s" } },
                "allowedNamespaces": { "all": true },
            },
            "status": { "phase": phase },
        }))
        .expect("valid ClusterRepository")
    }

    fn restore_cr(ns: &str, name: &str, phase: &str) -> Restore {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Restore",
            "metadata": { "name": name, "namespace": ns },
            "spec": {
                "source": { "snapshotRef": { "name": "b" } },
                "target": { "pvcRef": { "name": "t" } },
            },
            "status": { "phase": phase },
        }))
        .expect("valid Restore")
    }

    /// Register the observers over four stores, defaulting the three we don't care
    /// about to empty. Returns the gathered exposition text.
    fn gather_with(m: &Metrics, stores: ResourceStores) -> String {
        m.register_resource_observers(stores);
        String::from_utf8(m.gather()).unwrap()
    }

    #[test]
    fn metrics_register_and_export_under_kopiur_namespace() {
        let m = Metrics::new();
        m.record_reconcile("Snapshot", 0.1);
        m.record_error("Snapshot", "transient");
        m.inc_orphaned_snapshot("ns");
        m.set_snapshot_verified("ns", "db", 1_700_000_000);
        m.set_repository_maintenance_configured("Repository", "ns", "nas", false);

        // The per-Snapshot series now come from a store, not a setter.
        let (_, repos, crepos, restores) = empty_stores();
        let snaps = make_store(vec![snapshot_cr(
            "ns",
            "db",
            Some("daily"),
            succeeded_status(1234, 5, END_TIME),
        )]);
        let text = gather_with(
            &m,
            ResourceStores {
                snapshots: snaps.0,
                repositories: repos.0,
                cluster_repositories: crepos.0,
                restores: restores.0,
            },
        );
        // The Prometheus exporter appends `_total` to counters.
        assert!(
            text.contains("kopiur_controller_reconciliations_total"),
            "{text}"
        );
        assert!(text.contains("kopiur_orphaned_snapshots_total"), "{text}");
        assert!(text.contains("kopiur_resource_phase"), "{text}");
        assert!(text.contains("kopiur_snapshot_size_bytes"), "{text}");
        assert!(
            text.contains("kopiur_snapshot_verified_timestamp_seconds"),
            "{text}"
        );
        assert!(
            text.contains("kopiur_repository_maintenance_configured"),
            "{text}"
        );
    }

    #[test]
    fn backup_refusals_export_with_the_reason_label() {
        // The refusal counter is the dashboard-visible side of a policy
        // refusal (read-only repo / ungated privileged mover) — the reconcile
        // itself returns Ok, so kopiur_controller_reconcile_errors_total never
        // sees it and this counter is the only aggregate signal.
        let m = Metrics::new();
        m.inc_backup_refused("apps", "db-daily", "RepositoryReadOnly");
        let text = String::from_utf8(m.gather()).unwrap();
        assert!(text.contains("kopiur_snapshot_refusals_total"), "{text}");
        assert!(text.contains("reason=\"RepositoryReadOnly\""), "{text}");
        assert!(text.contains("name=\"db-daily\""), "{text}");
    }

    /// A live Succeeded Snapshot emits exactly one phase line (value 1,
    /// phase="Succeeded", policy=…), never a 0-valued phase series, plus the stat
    /// series carrying the policy label.
    #[test]
    fn succeeded_snapshot_emits_single_active_phase_and_stats() {
        let m = Metrics::new();
        let (_, repos, crepos, restores) = empty_stores();
        let snaps = make_store(vec![snapshot_cr(
            "apps",
            "db",
            Some("daily"),
            succeeded_status(4321, 42, END_TIME),
        )]);
        let text = gather_with(
            &m,
            ResourceStores {
                snapshots: snaps.0,
                repositories: repos.0,
                cluster_repositories: crepos.0,
                restores: restores.0,
            },
        );

        let phase_lines: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("kopiur_resource_phase{") && l.contains("name=\"db\""))
            .collect();
        assert_eq!(phase_lines.len(), 1, "exactly one phase series: {text}");
        let line = phase_lines[0];
        assert!(line.contains("phase=\"Succeeded\""), "{line}");
        assert!(line.contains("policy=\"daily\""), "{line}");
        assert!(line.trim_end().ends_with(" 1"), "active phase is 1: {line}");
        // No 0-valued phase series for any inactive phase.
        assert!(
            !text.lines().any(|l| l.starts_with("kopiur_resource_phase{")
                && l.contains("name=\"db\"")
                && l.trim_end().ends_with(" 0")),
            "no 0-valued phase series: {text}"
        );

        // Stat series present, carrying the policy label + the mover-recorded endTime.
        for metric in [
            "kopiur_snapshot_size_bytes",
            "kopiur_snapshot_files",
            "kopiur_snapshot_duration_seconds",
            "kopiur_snapshot_last_success_timestamp_seconds",
        ] {
            let l = text
                .lines()
                .find(|l| l.starts_with(&format!("{metric}{{")) && l.contains("name=\"db\""))
                .unwrap_or_else(|| panic!("missing {metric}: {text}"));
            assert!(l.contains("policy=\"daily\""), "{l}");
        }
        // last_success is the endTime, not now().
        let ts_line = text
            .lines()
            .find(|l| l.starts_with("kopiur_snapshot_last_success_timestamp_seconds{"))
            .unwrap();
        assert!(
            ts_line.trim_end().ends_with(&END_UNIX.to_string()),
            "{ts_line}"
        );
    }

    /// The #172/#175 regression pin: after the Snapshot CR is deleted, every one of
    /// its series is absent on the next collection (a sync gauge could only zero
    /// them, never drop them).
    #[test]
    fn deleted_snapshot_series_disappear() {
        let m = Metrics::new();
        let cr = snapshot_cr(
            "apps",
            "db",
            Some("daily"),
            succeeded_status(4321, 42, END_TIME),
        );
        let (reader, mut writer) = reflector::store::<Snapshot>();
        writer.apply_watcher_event(&watcher::Event::Apply(cr.clone()));
        let (_, repos, crepos, restores) = empty_stores();
        m.register_resource_observers(ResourceStores {
            snapshots: reader,
            repositories: repos.0,
            cluster_repositories: crepos.0,
            restores: restores.0,
        });

        let before = String::from_utf8(m.gather()).unwrap();
        assert!(
            before.contains("name=\"db\""),
            "present before delete: {before}"
        );

        // Delete the CR from the store the callbacks read.
        writer.apply_watcher_event(&watcher::Event::Delete(cr));
        let after = String::from_utf8(m.gather()).unwrap();
        assert!(
            !after.contains("name=\"db\""),
            "all series for the deleted Snapshot must disappear: {after}"
        );
    }

    /// The per-policy "latest" family reflects the newest Succeeded snapshot; a newer
    /// Failed snapshot never displaces the older Succeeded winner.
    #[test]
    fn policy_last_backup_reflects_newest_succeeded() {
        let m = Metrics::new();
        let older = snapshot_cr(
            "apps",
            "db-1",
            Some("daily"),
            succeeded_status(100, 10, "2026-05-24T00:00:00Z"),
        );
        let newer = snapshot_cr(
            "apps",
            "db-2",
            Some("daily"),
            succeeded_status(200, 20, END_TIME),
        );
        // A Failed snapshot even newer than the Succeeded winner — must be ignored.
        let failed_newer = snapshot_cr(
            "apps",
            "db-3",
            Some("daily"),
            serde_json::json!({
                "phase": "Failed",
                "timing": { "endTime": "2027-01-01T00:00:00Z", "durationSeconds": 99 },
                "stats": { "sizeBytes": 999 },
            }),
        );
        let (_, repos, crepos, restores) = empty_stores();
        let snaps = make_store(vec![older, newer, failed_newer]);
        let text = gather_with(
            &m,
            ResourceStores {
                snapshots: snaps.0,
                repositories: repos.0,
                cluster_repositories: crepos.0,
                restores: restores.0,
            },
        );

        let val = |metric: &str| -> String {
            text.lines()
                .find(|l| l.starts_with(&format!("{metric}{{")) && l.contains("policy=\"daily\""))
                .unwrap_or_else(|| panic!("missing {metric}: {text}"))
                .rsplit(' ')
                .next()
                .unwrap()
                .to_string()
        };
        assert_eq!(val("kopiur_policy_last_backup_size_bytes"), "200");
        assert_eq!(val("kopiur_policy_last_backup_duration_seconds"), "20");
        assert_eq!(
            val("kopiur_policy_last_backup_success_timestamp_seconds"),
            END_UNIX.to_string()
        );
    }

    /// A discovered Snapshot (no policyRef) gets no `policy` label and never feeds
    /// the per-policy family.
    #[test]
    fn snapshot_without_policyref_omits_policy_label() {
        let m = Metrics::new();
        let (_, repos, crepos, restores) = empty_stores();
        let snaps = make_store(vec![snapshot_cr(
            "apps",
            "orphan",
            None,
            succeeded_status(50, 5, END_TIME),
        )]);
        let text = gather_with(
            &m,
            ResourceStores {
                snapshots: snaps.0,
                repositories: repos.0,
                cluster_repositories: crepos.0,
                restores: restores.0,
            },
        );
        let phase_line = text
            .lines()
            .find(|l| l.starts_with("kopiur_resource_phase{") && l.contains("name=\"orphan\""))
            .unwrap();
        assert!(
            !phase_line.contains("policy="),
            "no policy label: {phase_line}"
        );
        assert!(
            !text.contains("kopiur_policy_last_backup"),
            "no per-policy family for a policy-less snapshot: {text}"
        );
    }

    /// Repository / ClusterRepository / Restore each emit a phase series with their
    /// kind and no `policy` label.
    #[test]
    fn repo_cluster_restore_emit_phase_without_policy() {
        let m = Metrics::new();
        let snaps: (Store<Snapshot>, Writer<Snapshot>) = make_store(vec![]);
        let repos = make_store(vec![repository_cr("ns", "nas", "Ready")]);
        let crepos = make_store(vec![cluster_repository_cr("shared", "Ready")]);
        let restores = make_store(vec![restore_cr("ns", "rst", "Restoring")]);
        let text = gather_with(
            &m,
            ResourceStores {
                snapshots: snaps.0,
                repositories: repos.0,
                cluster_repositories: crepos.0,
                restores: restores.0,
            },
        );
        for (kind, name) in [
            ("Repository", "nas"),
            ("ClusterRepository", "shared"),
            ("Restore", "rst"),
        ] {
            let line = text
                .lines()
                .find(|l| {
                    l.starts_with("kopiur_resource_phase{")
                        && l.contains(&format!("kind=\"{kind}\""))
                        && l.contains(&format!("name=\"{name}\""))
                })
                .unwrap_or_else(|| panic!("missing {kind}/{name}: {text}"));
            assert!(
                !line.contains("policy="),
                "{kind} has no policy label: {line}"
            );
            assert!(line.trim_end().ends_with(" 1"), "{line}");
        }
    }

    #[test]
    fn completion_counter_exports_with_labels() {
        let m = Metrics::new();
        m.inc_snapshot_completed("succeeded", "apps", Some("daily"));
        m.inc_snapshot_completed("failed", "apps", None);
        let text = String::from_utf8(m.gather()).unwrap();
        assert!(text.contains("kopiur_snapshots_completed_total"), "{text}");
        let succ = text
            .lines()
            .find(|l| {
                l.starts_with("kopiur_snapshots_completed_total{")
                    && l.contains("result=\"succeeded\"")
            })
            .unwrap();
        assert!(succ.contains("policy=\"daily\""), "{succ}");
        assert!(succ.contains("namespace=\"apps\""), "{succ}");
        // A policy-less failure omits the policy label.
        let fail = text
            .lines()
            .find(|l| {
                l.starts_with("kopiur_snapshots_completed_total{")
                    && l.contains("result=\"failed\"")
            })
            .unwrap();
        assert!(!fail.contains("policy="), "{fail}");
    }

    // ---- pure-function tests ----------------------------------------------

    fn stats(new: Option<i64>, modified: Option<i64>, unchanged: Option<i64>) -> SnapshotStats {
        serde_json::from_value(serde_json::json!({
            "filesNew": new, "filesModified": modified, "filesUnchanged": unchanged,
        }))
        .unwrap()
    }

    #[test]
    fn file_count_rule_unknown_is_absent() {
        // Nothing recorded ⇒ None (not a bogus 0).
        assert_eq!(snapshot_file_count(&stats(None, None, None)), None);
        // Any category present ⇒ Some, missing categories treated as 0.
        assert_eq!(snapshot_file_count(&stats(Some(3), None, None)), Some(3));
        assert_eq!(
            snapshot_file_count(&stats(Some(3), Some(1), Some(6))),
            Some(10)
        );
        // All-zero-but-present is a measured 0, not unknown.
        assert_eq!(
            snapshot_file_count(&stats(Some(0), Some(0), Some(0))),
            Some(0)
        );
    }

    #[test]
    fn latest_per_policy_edge_cases() {
        // Empty.
        assert!(latest_per_policy(&[]).is_empty());

        // All failed ⇒ nothing.
        let all_failed = vec![Arc::new(snapshot_cr(
            "apps",
            "x",
            Some("daily"),
            serde_json::json!({ "phase": "Failed", "timing": { "endTime": END_TIME } }),
        ))];
        assert!(latest_per_policy(&all_failed).is_empty());

        // Missing endTime ⇒ excluded.
        let no_end = vec![Arc::new(snapshot_cr(
            "apps",
            "x",
            Some("daily"),
            serde_json::json!({ "phase": "Succeeded", "stats": { "sizeBytes": 1 } }),
        ))];
        assert!(latest_per_policy(&no_end).is_empty());

        // No policyRef ⇒ excluded.
        let no_policy = vec![Arc::new(snapshot_cr(
            "apps",
            "x",
            None,
            succeeded_status(1, 1, END_TIME),
        ))];
        assert!(latest_per_policy(&no_policy).is_empty());

        // Two policies in two namespaces ⇒ one row each, deterministic order.
        let snaps = vec![
            Arc::new(snapshot_cr(
                "b-ns",
                "s1",
                Some("p2"),
                succeeded_status(2, 2, END_TIME),
            )),
            Arc::new(snapshot_cr(
                "a-ns",
                "s2",
                Some("p1"),
                succeeded_status(1, 1, END_TIME),
            )),
        ];
        let out = latest_per_policy(&snaps);
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].namespace.as_str(), out[0].policy.as_str()),
            ("a-ns", "p1")
        );
        assert_eq!(
            (out[1].namespace.as_str(), out[1].policy.as_str()),
            ("b-ns", "p2")
        );
    }
}
