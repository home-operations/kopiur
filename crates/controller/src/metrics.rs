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

use kopiur_api::common::{RepositoryKind, RepositoryRef};
use kopiur_api::{
    ClusterRepository, PhaseLabel, Repository, Restore, Snapshot, SnapshotPhase, SnapshotStats,
};
use kopiur_telemetry::MetricsProvider;

use crate::consts::{DELETION_HELD_CONDITION, SNAPSHOT_CLEANUP_FINALIZER};

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
    failure_events_dropped: Counter<u64>,

    // Snapshot business metrics.
    //
    // Per-resource phase (`kopiur_resource_phase`), the per-Snapshot
    // size/files/duration/last-success gauges, and the per-policy
    // consecutive-failures/live-count/gated gauges are NOT held here: they are
    // store-backed i64 *observable* gauges registered in
    // [`Metrics::register_resource_observers`], whose callbacks enumerate the
    // controllers' reflector stores at collection time. A series exists iff its CR
    // does — the callback is the sole source of truth per collection cycle, so an
    // attribute set it doesn't observe is simply absent from that cycle's point
    // set, and a deleted CR's series genuinely disappears from `/metrics` (the
    // #172/#175 fix; a sync gauge could only zero a series, never remove it).
    backup_verified_timestamp: Gauge<i64>,
    snapshots_completed: Counter<u64>,
    snapshot_deletion_failures: Counter<u64>,
    orphaned_snapshots: Counter<u64>,
    snapshot_deletions: Counter<u64>,
    snapshots_cascade_retained: Counter<u64>,
    snapshots_policy_cascade_retained: Counter<u64>,
    policy_cascade_children_deleted: Counter<u64>,
    snapshot_delete_batch_jobs: Counter<u64>,
    snapshot_delete_batch_members: Counter<u64>,
    work_spec_cms_swept: Counter<u64>,
    projected_secrets_swept: Counter<u64>,
    creds_secrets_reaped: Counter<u64>,
    projected_secrets_live: Gauge<i64>,
    snapshots_adopted: Counter<u64>,
    schedule_backups_created: Counter<u64>,
    secrets_projected: Counter<u64>,
    backups_refused: Counter<u64>,
    health_probe_failures: Counter<u64>,
    breaker_trips: Counter<u64>,

    // Repository business metrics.
    repo_size_bytes: Gauge<i64>,
    repo_snapshot_count: Gauge<i64>,
    repo_discovered_backups: Gauge<i64>,
    repo_foreign_snapshots: Gauge<i64>,
    repo_maintenance_configured: Gauge<i64>,

    // Restore + maintenance.
    restore_duration_seconds: Gauge<i64>,
    maintenance_reclaimed_bytes: Gauge<i64>,

    // Leader election. #319 was invisible on /metrics — it could only be found
    // by grepping logs — so the election now reports its own health.
    leader_is_leader: Gauge<i64>,
    leader_transitions: Counter<u64>,
    leader_renew_failures: Counter<u64>,
    leader_renew_duration: Histogram<f64>,
}

/// Outcome of a `Snapshot`'s finalizer resolving its kopia snapshot lifecycle,
/// as recorded by `kopiur_snapshot_deletions`. An exhaustive, closed set (the
/// type-safety thesis): [`Metrics::inc_snapshot_deletion`] takes this enum,
/// never a free string, so a new outcome can't silently mint an unbounded
/// label value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotDeletionOutcome {
    /// The kopia snapshot was deleted (single or batch Job succeeded).
    Deleted,
    /// The kopia snapshot was kept (`deletionPolicy: Retain`, or an operator
    /// prune that resolved to `Retain`).
    Retained,
    /// The `Snapshot` CR was removed without touching the kopia snapshot
    /// (`deletionPolicy: Orphan`, or the skip-snapshot-cleanup annotation).
    Orphaned,
    /// Retained specifically because the schedule-deletion cascade guard fired
    /// (owning `SnapshotSchedule` gone/replaced, `onScheduleDelete: Retain`).
    /// Prefer [`Metrics::inc_snapshot_cascade_retained`] over passing this
    /// variant to `inc_snapshot_deletion` directly — it also bumps the
    /// narrower `kopiur_snapshots_cascade_retained` counter in one place.
    CascadeRetained,
    /// Retained specifically because the policy-deletion cascade fired
    /// (`pruned-by: policy-cascade` with the Snapshot's own effective
    /// `deletionPolicy: Delete`). Prefer
    /// [`Metrics::inc_snapshot_policy_cascade_retained`] over passing this
    /// variant to `inc_snapshot_deletion` directly — it also bumps the
    /// narrower `kopiur_snapshots_policy_cascade_retained` counter in one place.
    PolicyCascadeRetained,
}

impl SnapshotDeletionOutcome {
    /// The `outcome` label value.
    pub fn as_str(self) -> &'static str {
        match self {
            SnapshotDeletionOutcome::Deleted => "deleted",
            SnapshotDeletionOutcome::Retained => "retained",
            SnapshotDeletionOutcome::Orphaned => "orphaned",
            SnapshotDeletionOutcome::CascadeRetained => "cascade_retained",
            SnapshotDeletionOutcome::PolicyCascadeRetained => "policy_cascade_retained",
        }
    }
}

/// The `mode` label value for `kopiur_policy_cascade_children_deleted`: which
/// `SnapshotPolicy` deletion-cascade mode ([`kopiur_api::common::PolicyDeletePolicy`])
/// produced this action. Kept controller-local (mirroring
/// [`SnapshotDeletionOutcome`]) so the metric label is a closed set independent
/// of the API enum's wire representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyCascadeMode {
    /// `PolicyDeletePolicy::Retain`: the child was stamped `pruned-by:
    /// policy-cascade` then deleted (its kopia snapshot is never touched).
    Retain,
    /// `PolicyDeletePolicy::Delete`: the child was bare-deleted, unstamped,
    /// classifying as an ordinary external deletion.
    Delete,
}

impl PolicyCascadeMode {
    /// The `mode` label value.
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyCascadeMode::Retain => "retain",
            PolicyCascadeMode::Delete => "delete",
        }
    }
}

/// Whole-Job outcome recorded by `kopiur_snapshot_delete_batch_jobs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchJobOutcome {
    /// Every member the Job targeted was deleted.
    Succeeded,
    /// At least one member failed (the Job's terminal `Failed` phase).
    Failed,
}

impl BatchJobOutcome {
    /// The `outcome` label value.
    pub fn as_str(self) -> &'static str {
        match self {
            BatchJobOutcome::Succeeded => "succeeded",
            BatchJobOutcome::Failed => "failed",
        }
    }
}

/// Per-member outcome recorded by `kopiur_snapshot_delete_batch_members`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchMemberOutcome {
    /// This member's kopia snapshot was deleted.
    Deleted,
    /// This member's delete failed; the Job kept going for the rest.
    Failed,
}

impl BatchMemberOutcome {
    /// The `outcome` label value.
    pub fn as_str(self) -> &'static str {
        match self {
            BatchMemberOutcome::Deleted => "deleted",
            BatchMemberOutcome::Failed => "failed",
        }
    }
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
        let failure_events_dropped = m
            .u64_counter("kopiur_controller_failure_events_dropped")
            .with_description(
                "Reconcile-failure Warning Events dropped instead of published, by cause: \
                 transport = the kube client's connection is down so a publish is futile; \
                 saturated = too many in-flight publishes (best-effort, never queued); \
                 timeout = the publish stalled past its deadline. A burst here is the \
                 /metrics-visible signature of an apiserver outage.",
            )
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

        // --- leader election ---
        //
        // Issue #319 (the controller abdicating ~15×/day off ordinary API
        // latency) was diagnosable only by grepping logs for "abdicating": there
        // was no /metrics signal at all. `leader_renew_duration` is the one that
        // would have shown it in a single panel — a p99 pinned at the renew
        // deadline is the whole story — and the others make flapping alertable.
        let leader_is_leader = m
            .i64_gauge("kopiur_leader_is_leader")
            .with_description(
                "1 if this replica currently holds the election Lease, 0 otherwise. Also the \
                 flag to filter dashboards by: the store-backed resource gauges are \
                 leader-only, so a standby's /metrics legitimately omits them.",
            )
            .build();
        let leader_transitions = m
            .u64_counter("kopiur_leader_transitions")
            .with_description(
                "Total times this replica acquired the election Lease. A healthy single-leader \
                 deployment increments this once per process start; a climbing rate is \
                 election flapping.",
            )
            .build();
        let leader_renew_failures = m
            .u64_counter("kopiur_leader_renew_failures")
            .with_description(
                "Failed Lease renew attempts by reason (stalled|transport|conflict). These are \
                 retried inside the renew window and are NOT by themselves leadership loss — \
                 a sustained nonzero rate is the early warning that used to be invisible.",
            )
            .build();
        let leader_renew_duration = m
            .f64_histogram("kopiur_leader_renew_duration_seconds")
            .with_description(
                "Wall time of one Lease renew attempt, successful or not. Healthy is single-digit \
                 milliseconds; a p99 approaching the renew deadline means the election is one \
                 hiccup away from abdicating.",
            )
            .with_boundaries(vec![0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0])
            .build();

        let backup_verified_timestamp = m
            .i64_gauge("kopiur_snapshot_verified_timestamp_seconds")
            .with_description(
                "Unix timestamp of the most recent successful SnapshotPolicy verification \
                 (quick or deep), from status.lastVerified.",
            )
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
        let snapshot_deletions = m
            .u64_counter("kopiur_snapshot_deletions")
            .with_description(
                "Total Snapshot finalizer resolutions, by outcome (deleted|retained|orphaned| \
                 cascade_retained|policy_cascade_retained) and namespace. Distinct from \
                 kopiur_snapshot_deletion_failures_total, which counts kopia delete-call \
                 FAILURES during finalizer handling, not resolutions — never sum the two.",
            )
            .build();
        let snapshots_cascade_retained = m
            .u64_counter("kopiur_snapshots_cascade_retained")
            .with_description(
                "Total Snapshots retained specifically by the schedule-deletion cascade guard \
                 (onScheduleDelete: Retain when the owning SnapshotSchedule is gone/replaced). \
                 A narrower, always-alongside view of \
                 kopiur_snapshot_deletions{outcome=\"cascade_retained\"} — both are bumped \
                 together by inc_snapshot_cascade_retained so the two series can't drift apart.",
            )
            .build();
        let snapshots_policy_cascade_retained = m
            .u64_counter("kopiur_snapshots_policy_cascade_retained")
            .with_description(
                "Total Snapshots retained specifically by the policy-deletion cascade \
                 (pruned-by: policy-cascade with the Snapshot's own effective deletionPolicy: \
                 Delete, when the owning SnapshotPolicy is gone and onPolicyDelete: Retain). A \
                 narrower, always-alongside view of \
                 kopiur_snapshot_deletions{outcome=\"policy_cascade_retained\"} — both are \
                 bumped together by inc_snapshot_policy_cascade_retained so the two series can't \
                 drift apart.",
            )
            .build();
        let policy_cascade_children_deleted = m
            .u64_counter("kopiur_policy_cascade_children_deleted")
            .with_description(
                "Total Snapshot children acted on by a SnapshotPolicy deletion-cascade \
                 finalizer, by mode: retain = stamped pruned-by: policy-cascade then \
                 deleted (the kopia snapshot itself is never touched); delete = a bare \
                 unstamped delete, classifying as an ordinary external deletion subject \
                 to the per-repository mass-deletion breaker. A terminating child merely \
                 reclassified by a stamp-only pass is NOT counted here — its eventual \
                 resolution is already covered by kopiur_snapshots_policy_cascade_retained.",
            )
            .build();
        let snapshot_delete_batch_jobs = m
            .u64_counter("kopiur_snapshot_delete_batch_jobs")
            .with_description(
                "Total mass-deletion batch-delete mover Jobs, by outcome (succeeded|failed).",
            )
            .build();
        let snapshot_delete_batch_members = m
            .u64_counter("kopiur_snapshot_delete_batch_members")
            .with_description(
                "Total Snapshots resolved by a batch-delete Job, by member outcome \
                 (deleted|failed). A single Job's members are counted independently — one \
                 member's failure does not stop the others (kopiur_snapshot_delete_batch_jobs \
                 records the whole-Job outcome separately).",
            )
            .build();
        let work_spec_cms_swept = m
            .u64_counter("kopiur_work_spec_cms_swept")
            .with_description(
                "Total orphaned mover work-spec ConfigMaps deleted by the periodic sweep \
                 (ConfigMaps whose mover Job was already TTL-reaped).",
            )
            .build();
        let projected_secrets_swept = m
            .u64_counter("kopiur_projected_secrets_swept")
            .with_description(
                "Total legacy per-run projected credential Secrets deleted by the periodic \
                 sweep (pre-stable-naming copies whose mover Job is gone).",
            )
            .build();
        let creds_secrets_reaped = m
            .u64_counter("kopiur_creds_secrets_reaped")
            .with_description(
                "Total projected credential Secrets reaped once no mover Job could still \
                 load them, by mechanism: `by=terminal` is the consuming CR's reconciler \
                 (the fast path), `by=sweep` is the periodic backstop. Do NOT sum them — a \
                 steady non-zero `by=sweep` rate means the reconciler's reap is not firing.",
            )
            .build();
        let projected_secrets_live = m
            .i64_gauge("kopiur_projected_secrets_live")
            .with_description(
                "Projected credential Secrets currently alive, observed each sweep pass. \
                 The population, not a delta: a counter of projections rises identically \
                 whether or not copies are ever reclaimed, which is why the per-run leak \
                 (#240) ran unseen. Alert on deriv() > 0 over a day.",
            )
            .build();
        let snapshots_adopted = m
            .u64_counter("kopiur_snapshots_adopted")
            .with_description(
                "Total discovered Snapshots auto-adopted into an identity-matching \
                 SnapshotPolicy (status.origin flipped to Adopted), by namespace and policy. \
                 Adopted rows are then GFS-governed and eventually pruned by the policy's \
                 spec.retention — opt out with spec.adoption: Ignore on the policy or \
                 spec.catalog.adoption: Ignore on the repository.",
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
        let breaker_trips = m
            .u64_counter("kopiur_repository_breaker_trips")
            .with_description(
                "Total repository circuit-breaker openings: the backend health probe exceeded \
                 spec.health.probe.failureThreshold under onFailure: Degrade, moving the \
                 repository to Degraded (backups, maintenance, and replication pause until a \
                 connect succeeds; recovery is automatic). Labeled by kind \
                 (Repository/ClusterRepository), namespace, name, and probe_kind \
                 (vanished/unreachable — matching the health-probe-failure outcome label).",
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
        let repo_foreign_snapshots = m
            .i64_gauge("kopiur_repo_foreign_snapshots")
            .with_description(
                "Snapshots in the repository catalog classified as another cluster's \
                 (multi-cluster shared repository; identityDefaults.cluster).",
            )
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
            failure_events_dropped,
            reconcile_duration,
            backup_verified_timestamp,
            snapshots_completed,
            snapshot_deletion_failures,
            orphaned_snapshots,
            snapshot_deletions,
            snapshots_cascade_retained,
            snapshots_policy_cascade_retained,
            policy_cascade_children_deleted,
            snapshot_delete_batch_jobs,
            snapshot_delete_batch_members,
            work_spec_cms_swept,
            projected_secrets_swept,
            creds_secrets_reaped,
            projected_secrets_live,
            snapshots_adopted,
            schedule_backups_created,
            secrets_projected,
            backups_refused,
            health_probe_failures,
            breaker_trips,
            repo_size_bytes,
            repo_snapshot_count,
            repo_discovered_backups,
            repo_foreign_snapshots,
            repo_maintenance_configured,
            restore_duration_seconds,
            leader_is_leader,
            leader_transitions,
            leader_renew_failures,
            leader_renew_duration,
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

    /// Count a reconcile-failure Warning Event that was dropped instead of
    /// published. `cause` ∈ {"transport", "saturated", "timeout"} — see the
    /// counter description; a burst is the /metrics-visible signature of an
    /// apiserver outage (when Events themselves cannot get through).
    pub fn record_failure_event_dropped(&self, cause: &'static str) {
        self.failure_events_dropped
            .add(1, &[KeyValue::new("cause", cause)]);
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
        // Per-policy backup HEALTH (issue #196): 1 when a policy's latest terminal
        // backup succeeded, 0 when it failed. `count(... == 1)` answers "how many of
        // my N policies are currently healthy" — a per-policy STATE the durable
        // `kopiur_snapshots_completed_total` run-counter cannot, since summing it over
        // a window counts every run (23 policies × N runs), not the 23 the user
        // expects for a healthy fleet. Store-backed, so a deleted policy's series
        // disappears on the next collection with no explicit cleanup.
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_snapshotpolicy_last_backup_success")
                .with_description(
                    "1 if a policy's most recent terminal backup succeeded, 0 if it failed, \
                     per (namespace, policy). Absent for a policy with no terminal backup yet.",
                )
                .with_callback(move |o| {
                    for h in policy_backup_health(&snapshots.state()) {
                        let attrs = [
                            KeyValue::new("namespace", h.namespace.clone()),
                            KeyValue::new("policy", h.policy.clone()),
                        ];
                        o.observe(i64::from(h.healthy), &attrs);
                    }
                })
                .build();
        }
        // Mass-deletion breaker observability (M3 plumbing; M4 wires the
        // enforcement itself). Deliberately cheap and store-only: counts any
        // deleting Snapshot that still carries the cleanup finalizer, grouped
        // by its `status.resolved.repository` pin (unpinned → the single
        // "unknown"/"unknown" bucket, never a per-repo guess). This is
        // COARSER than the breaker's own count — it does not exclude operator
        // prunes and never re-runs `plan_deletion` (which needs the
        // SnapshotSchedule store this callback doesn't have) — so it may read
        // higher than the threshold-relevant count. These are observability
        // gauges, not the enforcement path.
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_snapshot_deletions_pending_external")
                .with_description(
                    "Snapshots being deleted (deletionTimestamp set) that still carry the \
                     cleanup finalizer, by resolved repository (repo_kind/repo_name; unpinned \
                     → repo_kind=repo_name=\"unknown\"). A coarser, cheaper approximation of \
                     the mass-deletion breaker's own count: unlike the breaker this includes \
                     operator prunes and does not re-run plan_deletion, so it may read higher \
                     than the threshold-relevant count.",
                )
                .with_callback(move |o| {
                    for s in snapshots.state() {
                        if !counts_toward_deletion_gauge(&s) {
                            continue;
                        }
                        o.observe(1, &repo_gauge_attrs(crate::snapshot::pinned_repository(&s)));
                    }
                })
                .build();
        }
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_snapshot_deletions_held")
                .with_description(
                    "Snapshots currently HELD by the mass-deletion breaker (DeletionHeld=True \
                     condition), by resolved repository (repo_kind/repo_name; unpinned → \
                     repo_kind=repo_name=\"unknown\"). A subset of \
                     kopiur_snapshot_deletions_pending_external.",
                )
                .with_callback(move |o| {
                    for s in snapshots.state() {
                        if !counts_toward_deletion_gauge(&s) || !deletion_held(&s) {
                            continue;
                        }
                        o.observe(1, &repo_gauge_attrs(crate::snapshot::pinned_repository(&s)));
                    }
                })
                .build();
        }
        // Per-policy consecutive-failure streak + live-CR count (M6, #345). These
        // were sync gauges written from the SnapshotPolicy reconcile, which left a
        // deleted policy's series behind forever (the #172/#175 defect class) and
        // froze the streak whenever the reconcile stopped running. Store-backed,
        // they are re-derived from the Snapshot store each collection: a series
        // exists iff the policy currently has >= 1 Snapshot CR carrying its
        // `spec.policyRef` (the same policy attribution as `kopiur_resource_phase`),
        // and it vanishes with the last CR. Labels stay {namespace, name} with
        // `name` = the SnapshotPolicy name, for series continuity with the old
        // sync gauges.
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_snapshot_consecutive_failures")
                .with_description(
                    "Consecutive Failed backups since the latest Succeeded one, per \
                     SnapshotPolicy (labels namespace/name, name = the policy name). \
                     Store-backed: the series exists iff the policy currently has at \
                     least one Snapshot CR (spec.policyRef attribution), and is \
                     re-derived from those CRs each collection.",
                )
                .with_callback(move |o| {
                    for p in policy_snapshot_counts(&snapshots.state()) {
                        o.observe(p.consecutive_failures, &ns_name(&p.namespace, &p.policy));
                    }
                })
                .build();
        }
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_snapshots_live")
                .with_description(
                    "Snapshot CRs currently alive per SnapshotPolicy (labels \
                     namespace/name, name = the policy name). Bounded by GFS retention \
                     when `spec.retention` is set; a policy WITHOUT it never prunes (a \
                     deliberate safe default) and this is the only thing that will tell \
                     you so. Store-backed: the series exists iff the policy currently \
                     has at least one Snapshot CR (spec.policyRef attribution).",
                )
                .with_callback(move |o| {
                    for p in policy_snapshot_counts(&snapshots.state()) {
                        o.observe(p.live, &ns_name(&p.namespace, &p.policy));
                    }
                })
                .build();
        }
        // Snapshots parked behind a not-Ready repository (M6, #345): the circuit
        // breaker holds them Pending (Ready reason `RepositoryNotReady`) rather
        // than refusing them, so this is the "deferred work" population that
        // drains automatically once the repository recovers.
        {
            let snapshots = stores.snapshots.clone();
            let _ = m
                .i64_observable_gauge("kopiur_snapshot_gated")
                .with_description(
                    "Snapshots parked behind a not-Ready repository (deferrals, not \
                     refusals): phase Pending with Ready reason RepositoryNotReady, \
                     counted per (namespace, policy); the policy label is omitted for \
                     Snapshots without a policyRef. Store-backed: the series drains \
                     to absence as the parked Snapshots launch or disappear.",
                )
                .with_callback(move |o| {
                    for g in gated_snapshot_counts(&snapshots.state()) {
                        let mut attrs = vec![KeyValue::new("namespace", g.namespace.clone())];
                        if let Some(policy) = g.policy.as_ref() {
                            attrs.push(KeyValue::new("policy", policy.clone()));
                        }
                        o.observe(g.count, &attrs);
                    }
                })
                .build();
        }
        // Repository circuit-breaker observability (M6, #345), over BOTH
        // repository stores. `kind`/`namespace`/`name` labeling matches
        // `kopiur_resource_phase`: a ClusterRepository carries namespace="".
        //
        // The consecutive-failures gauge is emitted whenever `status.health`
        // exists (the probe has run at least once): a 0 after recovery is the
        // informative "streak cleared" signal, and the store-backed philosophy —
        // a series exists iff it is meaningful, and dies with the CR — is
        // preserved because absence of health status (probe disabled / never
        // ran) is genuinely "unknown", not a healthy 0.
        {
            let repositories = stores.repositories.clone();
            let cluster_repositories = stores.cluster_repositories.clone();
            let _ = m
                .i64_observable_gauge("kopiur_repository_consecutive_backend_failures")
                .with_description(
                    "Consecutive failed backend connects (health probe + strict \
                     retries) from status.health.consecutiveProbeFailures, per \
                     repository (kind/namespace/name; namespace is empty for a \
                     ClusterRepository). Emitted whenever health status exists — a 0 \
                     after recovery is informative — and absent when the probe has \
                     never run; the series dies with the CR.",
                )
                .with_callback(move |o| {
                    for r in repositories.state() {
                        let Some(st) = r.status.as_ref() else {
                            continue;
                        };
                        if let Some(n) = repo_consecutive_backend_failures(st.health.as_ref()) {
                            o.observe(
                                n,
                                &repo_health_attrs(
                                    "Repository",
                                    &r.namespace().unwrap_or_default(),
                                    &r.name_any(),
                                ),
                            );
                        }
                    }
                    for r in cluster_repositories.state() {
                        let Some(st) = r.status.as_ref() else {
                            continue;
                        };
                        if let Some(n) = repo_consecutive_backend_failures(st.health.as_ref()) {
                            o.observe(
                                n,
                                &repo_health_attrs("ClusterRepository", "", &r.name_any()),
                            );
                        }
                    }
                })
                .build();
        }
        // Breaker-open marker: the series EXISTS only while the breaker is open
        // (phase Degraded + BackendReachable=False) and its value is the episode
        // start (status.health.firstFailureAt), so `time() - metric` is "how long
        // has this breaker been open" and recovery removes the series entirely.
        {
            let repositories = stores.repositories.clone();
            let cluster_repositories = stores.cluster_repositories.clone();
            let _ = m
                .i64_observable_gauge("kopiur_repository_breaker_open_since_timestamp_seconds")
                .with_description(
                    "Unix timestamp of status.health.firstFailureAt, emitted ONLY \
                     while the repository circuit breaker is open (phase Degraded \
                     with BackendReachable=False), per repository (kind/namespace/ \
                     name; namespace is empty for a ClusterRepository). The series \
                     disappears when the breaker closes.",
                )
                .with_callback(move |o| {
                    for r in repositories.state() {
                        let Some(st) = r.status.as_ref() else {
                            continue;
                        };
                        if let Some(ts) =
                            breaker_open_since(st.phase, st.health.as_ref(), &st.conditions)
                        {
                            o.observe(
                                ts,
                                &repo_health_attrs(
                                    "Repository",
                                    &r.namespace().unwrap_or_default(),
                                    &r.name_any(),
                                ),
                            );
                        }
                    }
                    for r in cluster_repositories.state() {
                        let Some(st) = r.status.as_ref() else {
                            continue;
                        };
                        if let Some(ts) =
                            breaker_open_since(st.phase, st.health.as_ref(), &st.conditions)
                        {
                            o.observe(
                                ts,
                                &repo_health_attrs("ClusterRepository", "", &r.name_any()),
                            );
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

    /// Count a `Snapshot` finalizer resolution in `namespace` with the given
    /// [`SnapshotDeletionOutcome`]. For [`SnapshotDeletionOutcome::CascadeRetained`],
    /// prefer [`Self::inc_snapshot_cascade_retained`], which calls this AND bumps
    /// the narrower cascade counter in one place.
    pub fn inc_snapshot_deletion(&self, ns: &str, outcome: SnapshotDeletionOutcome) {
        self.snapshot_deletions.add(
            1,
            &[
                KeyValue::new("namespace", ns.to_string()),
                KeyValue::new("outcome", outcome.as_str()),
            ],
        );
    }

    /// Record a `Snapshot` retained by the schedule-deletion cascade guard:
    /// bumps BOTH `kopiur_snapshot_deletions{outcome="cascade_retained"}` and
    /// the narrower `kopiur_snapshots_cascade_retained`, in the one place, so
    /// the two series can never drift apart. Call this INSTEAD OF
    /// `inc_snapshot_deletion(ns, SnapshotDeletionOutcome::CascadeRetained)`.
    pub fn inc_snapshot_cascade_retained(&self, ns: &str) {
        self.inc_snapshot_deletion(ns, SnapshotDeletionOutcome::CascadeRetained);
        self.snapshots_cascade_retained
            .add(1, &[KeyValue::new("namespace", ns.to_string())]);
    }

    /// Record a `Snapshot` retained by the policy-deletion cascade: bumps BOTH
    /// `kopiur_snapshot_deletions{outcome="policy_cascade_retained"}` and the
    /// narrower `kopiur_snapshots_policy_cascade_retained`, in the one place,
    /// so the two series can never drift apart. Call this INSTEAD OF
    /// `inc_snapshot_deletion(ns, SnapshotDeletionOutcome::PolicyCascadeRetained)`.
    pub fn inc_snapshot_policy_cascade_retained(&self, ns: &str) {
        self.inc_snapshot_deletion(ns, SnapshotDeletionOutcome::PolicyCascadeRetained);
        self.snapshots_policy_cascade_retained
            .add(1, &[KeyValue::new("namespace", ns.to_string())]);
    }

    /// Count one `Snapshot` child acted on by the `SnapshotPolicy`
    /// deletion-cascade finalizer in `namespace`, under `mode`
    /// (`stamp_and_delete` under Retain, bare `delete_only` under Delete —
    /// NOT the non-blocking `stamp_only` reclassification).
    pub fn inc_policy_cascade_children_deleted(&self, ns: &str, mode: PolicyCascadeMode) {
        self.policy_cascade_children_deleted.add(
            1,
            &[
                KeyValue::new("namespace", ns.to_string()),
                KeyValue::new("mode", mode.as_str()),
            ],
        );
    }

    /// Count one mass-deletion batch-delete Job reaching a terminal outcome.
    pub fn inc_snapshot_delete_batch_job(&self, outcome: BatchJobOutcome) {
        self.snapshot_delete_batch_jobs
            .add(1, &[KeyValue::new("outcome", outcome.as_str())]);
    }

    /// Count `n` batch-delete Job members resolving to `outcome`.
    pub fn inc_snapshot_delete_batch_members(&self, outcome: BatchMemberOutcome, n: u64) {
        self.snapshot_delete_batch_members
            .add(n, &[KeyValue::new("outcome", outcome.as_str())]);
    }

    /// Count `n` orphaned work-spec ConfigMaps deleted by one sweep pass.
    pub fn inc_work_spec_cms_swept(&self, n: u64) {
        self.work_spec_cms_swept.add(n, &[]);
    }

    /// Count `n` legacy per-run projected credential Secrets deleted by one
    /// sweep pass.
    pub fn inc_projected_secrets_swept(&self, n: u64) {
        self.projected_secrets_swept.add(n, &[]);
    }

    /// Count `n` projected credential Secrets reaped once no mover Job could still
    /// load them. `by` is the mechanism — `terminal` (the consuming CR's reconciler)
    /// or `sweep` (the periodic backstop) — and the two are deliberately NOT summed:
    /// in steady state the reconciler should get there first, so a persistent
    /// `by=sweep` rate is the signal that its reap has stopped firing.
    pub fn inc_creds_secrets_reaped(&self, by: &'static str, n: u64) {
        self.creds_secrets_reaped
            .add(n, &[opentelemetry::KeyValue::new("by", by)]);
    }

    /// Record the projected credential Secrets alive after a sweep pass.
    pub fn set_projected_secrets_live(&self, n: i64) {
        self.projected_secrets_live.record(n, &[]);
    }

    /// Count a Snapshot CR created by a SnapshotSchedule.
    pub fn inc_schedule_backup_created(&self, ns: &str, name: &str) {
        self.schedule_backups_created.add(1, &ns_name(ns, name));
    }

    /// Count `n` discovered Snapshots auto-adopted into a `SnapshotPolicy`
    /// (labels: `namespace`, `policy`). Called once per adoption wave with the
    /// wave's count.
    pub fn inc_snapshots_adopted(&self, ns: &str, policy: &str, n: u64) {
        self.snapshots_adopted.add(
            n,
            &[
                KeyValue::new("namespace", ns.to_string()),
                KeyValue::new("policy", policy.to_string()),
            ],
        );
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

    /// Count a circuit-breaker opening (#345 M4): the repository transitioned to
    /// `Degraded` because the backend probe exceeded `failureThreshold` under
    /// `onFailure: Degrade`. Fired on the TRANSITION only (never on re-confirmed
    /// failures while already open). `kind` is `Repository`/`ClusterRepository`
    /// (`ns` empty for the latter); `probe_kind` is `vanished`/`unreachable`
    /// ([`crate::health::ProbeFailureKind::label`]), matching the
    /// health-probe-failure `outcome` label so the two counters join.
    pub fn inc_breaker_trip(&self, kind: &str, ns: &str, name: &str, probe_kind: &str) {
        self.breaker_trips.add(
            1,
            &[
                KeyValue::new("kind", kind.to_string()),
                KeyValue::new("namespace", ns.to_string()),
                KeyValue::new("name", name.to_string()),
                KeyValue::new("probe_kind", probe_kind.to_string()),
            ],
        );
    }

    // ---- repository / restore / maintenance --------------------------------

    /// Set the repository size gauge.
    pub fn set_repo_size_bytes(&self, ns: &str, name: &str, bytes: i64) {
        self.repo_size_bytes.record(bytes, &ns_name(ns, name));
    }

    /// Set the repository snapshot-count, discovered-backup, and foreign-snapshot
    /// gauges (`foreign`: multi-cluster shared repo, `status.catalog.foreignSnapshotCount`).
    pub fn set_repo_catalog(
        &self,
        ns: &str,
        name: &str,
        snapshot_count: Option<i64>,
        discovered: Option<i64>,
        foreign: Option<i64>,
    ) {
        let labels = ns_name(ns, name);
        if let Some(v) = snapshot_count {
            self.repo_snapshot_count.record(v, &labels);
        }
        if let Some(v) = discovered {
            self.repo_discovered_backups.record(v, &labels);
        }
        if let Some(v) = foreign {
            self.repo_foreign_snapshots.record(v, &labels);
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

    /// Record whether this replica currently holds the election Lease, bumping
    /// the transition counter on each acquisition.
    pub fn set_leader(&self, leading: bool) {
        self.leader_is_leader.record(leading as i64, &[]);
        if leading {
            self.leader_transitions.add(1, &[]);
        }
    }

    /// Record one renew attempt: how long it took, and why it failed if it did.
    ///
    /// `reason` is `None` on success. Kept as a closed set of `&'static str`
    /// at the call sites rather than free-form text — this is a metric label,
    /// and unbounded cardinality here would be a footgun on a busy cluster.
    pub fn record_leader_renew(&self, seconds: f64, reason: Option<&'static str>) {
        self.leader_renew_duration.record(seconds, &[]);
        if let Some(reason) = reason {
            self.leader_renew_failures
                .add(1, &[KeyValue::new("reason", reason)]);
        }
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

/// Whether a `Snapshot` counts toward the deletion-observability gauges
/// (`kopiur_snapshot_deletions_pending_external`/`_held`): it is being deleted
/// and still carries the cleanup finalizer. Deliberately NOT the
/// mass-deletion breaker's own predicate, which additionally excludes
/// operator prunes and needs the owning `SnapshotSchedule` — see the gauges'
/// registration comment for why these diverge. Pure so the counting rule is
/// unit-tested off-OTel.
fn counts_toward_deletion_gauge(backup: &Snapshot) -> bool {
    backup.metadata.deletion_timestamp.is_some()
        && backup
            .metadata
            .finalizers
            .as_ref()
            .is_some_and(|f| f.iter().any(|x| x == SNAPSHOT_CLEANUP_FINALIZER))
}

/// Whether a `Snapshot`'s deletion is currently HELD by the mass-deletion
/// breaker (`DeletionHeld=True`). Pure so it's unit-tested off-OTel.
fn deletion_held(backup: &Snapshot) -> bool {
    backup.status.as_ref().is_some_and(|s| {
        s.conditions
            .iter()
            .any(|c| c.type_ == DELETION_HELD_CONDITION && c.status == "True")
    })
}

/// `repo_kind`/`repo_name` attributes for the deletion-observability gauges:
/// the pinned repository's kind/name, or the single conservative
/// "unknown"/"unknown" bucket when unpinned — never a per-repo guess, and
/// never every-repo double-counting. Pure so the label mapping is
/// unit-tested off-OTel.
fn repo_gauge_attrs(pinned: Option<&RepositoryRef>) -> [KeyValue; 2] {
    let (kind, name) = match pinned {
        Some(r) => (
            match r.kind {
                RepositoryKind::Repository => "Repository",
                RepositoryKind::ClusterRepository => "ClusterRepository",
            },
            r.name.clone(),
        ),
        None => ("unknown", "unknown".to_string()),
    };
    [
        KeyValue::new("repo_kind", kind),
        KeyValue::new("repo_name", name),
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

/// Whether a policy's most recent *completed* backup succeeded (issue #196).
#[derive(Debug, Clone, PartialEq)]
struct PolicyHealth {
    namespace: String,
    policy: String,
    /// `true` when the latest terminal (Succeeded/Failed) Snapshot for this policy
    /// is Succeeded.
    healthy: bool,
}

/// Reduce a Snapshot set to, per (namespace, policy), whether that policy's LATEST
/// terminal backup succeeded — the signal behind `kopiur_snapshotpolicy_last_backup_success`.
///
/// This answers "of my N policies, how many currently have a healthy last backup"
/// (issue #196) — the question the durable `kopiur_snapshots_completed_total`
/// counter can't, because summing it over a window counts every *run*, not the
/// per-policy *state* (23 policies × N runs ≫ 23). "Latest" is by
/// `metadata.creationTimestamp` (always present, monotonic with attempt order), so
/// a policy whose newest attempt FAILED reports `0` even though an older run
/// succeeded. Only terminal (`Succeeded`/`Failed`) Snapshots carrying a `policyRef`
/// participate — an in-flight `Running`/`Pending` run never flips the state, and
/// `Discovered` snapshots (no `policyRef`) are excluded. A policy with no terminal
/// backup yet is simply absent (genuinely "unknown", not healthy or failed). Pure so
/// the reduction is unit-tested off-cluster; output sorted for deterministic tests.
fn policy_backup_health(snapshots: &[Arc<Snapshot>]) -> Vec<PolicyHealth> {
    use std::collections::HashMap;
    // (ns, policy) -> (creation_unix of the winning terminal snapshot, healthy).
    let mut best: HashMap<(String, String), (i64, bool)> = HashMap::new();
    for s in snapshots {
        let Some(policy) = s.spec.policy_ref.as_ref().map(|p| p.name.clone()) else {
            continue;
        };
        let healthy = match s.status.as_ref().and_then(|st| st.phase) {
            Some(SnapshotPhase::Succeeded) => true,
            Some(SnapshotPhase::Failed) => false,
            // Non-terminal / Discovered / Deleting never define "last backup".
            _ => continue,
        };
        let created = s
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0.as_second())
            .unwrap_or(0);
        let namespace = s.namespace().unwrap_or_default();
        best.entry((namespace, policy))
            .and_modify(|cur| {
                if created >= cur.0 {
                    *cur = (created, healthy);
                }
            })
            .or_insert((created, healthy));
    }
    let mut out: Vec<PolicyHealth> = best
        .into_iter()
        .map(|((namespace, policy), (_, healthy))| PolicyHealth {
            namespace,
            policy,
            healthy,
        })
        .collect();
    out.sort_by(|a, b| {
        (a.namespace.as_str(), a.policy.as_str()).cmp(&(b.namespace.as_str(), b.policy.as_str()))
    });
    out
}

/// Per-(namespace, policy) Snapshot population figures for the store-backed
/// `kopiur_snapshots_live` / `kopiur_snapshot_consecutive_failures` gauges.
#[derive(Debug, Clone, PartialEq)]
struct PolicySnapshots {
    namespace: String,
    policy: String,
    /// Snapshot CRs currently carrying this policy's `spec.policyRef`.
    live: i64,
    /// Trailing Failed streak before the latest Succeeded terminal backup
    /// ([`crate::snapshot_policy::consecutive_failures`]).
    consecutive_failures: i64,
}

/// Group a Snapshot set per (namespace, policy) — the same `spec.policyRef`
/// attribution as `kopiur_resource_phase` (Snapshots without a `policyRef` never
/// participate) — and reduce each group to its live count + consecutive-failure
/// streak. Pure so the store-derivation is unit-tested off-OTel; output sorted
/// for deterministic tests.
fn policy_snapshot_counts(snapshots: &[Arc<Snapshot>]) -> Vec<PolicySnapshots> {
    use std::collections::HashMap;
    let mut groups: HashMap<(String, String), Vec<&Snapshot>> = HashMap::new();
    for s in snapshots {
        let Some(policy) = s.spec.policy_ref.as_ref().map(|p| p.name.clone()) else {
            continue;
        };
        groups
            .entry((s.namespace().unwrap_or_default(), policy))
            .or_default()
            .push(s.as_ref());
    }
    let mut out: Vec<PolicySnapshots> = groups
        .into_iter()
        .map(|((namespace, policy), group)| PolicySnapshots {
            namespace,
            policy,
            live: group.len() as i64,
            consecutive_failures: crate::snapshot_policy::consecutive_failures(
                group.iter().copied(),
            ),
        })
        .collect();
    out.sort_by(|a, b| {
        (a.namespace.as_str(), a.policy.as_str()).cmp(&(b.namespace.as_str(), b.policy.as_str()))
    });
    out
}

/// Per-(namespace, policy) count of gated Snapshots for `kopiur_snapshot_gated`;
/// `policy` is `None` (label omitted) for Snapshots without a `policyRef`.
#[derive(Debug, Clone, PartialEq)]
struct GatedSnapshots {
    namespace: String,
    policy: Option<String>,
    count: i64,
}

/// Whether a `Snapshot` is currently parked behind a not-Ready repository:
/// phase `Pending` (or unset — a just-created CR defaults to `Pending`) AND the
/// `Ready` condition carries reason `RepositoryNotReady`. Pure so the gating
/// predicate is unit-tested off-OTel.
fn snapshot_gated(s: &Snapshot) -> bool {
    let phase = s.status.as_ref().and_then(|st| st.phase);
    if !matches!(phase, None | Some(SnapshotPhase::Pending)) {
        return false;
    }
    s.status.as_ref().is_some_and(|st| {
        st.conditions.iter().any(|c| {
            c.type_ == crate::consts::READY_CONDITION
                && c.reason == crate::consts::REPOSITORY_NOT_READY_REASON
        })
    })
}

/// Reduce a Snapshot set to the gated population per (namespace, policy). Pure;
/// output sorted for deterministic tests.
fn gated_snapshot_counts(snapshots: &[Arc<Snapshot>]) -> Vec<GatedSnapshots> {
    use std::collections::HashMap;
    let mut counts: HashMap<(String, Option<String>), i64> = HashMap::new();
    for s in snapshots {
        if !snapshot_gated(s) {
            continue;
        }
        let policy = s.spec.policy_ref.as_ref().map(|p| p.name.clone());
        *counts
            .entry((s.namespace().unwrap_or_default(), policy))
            .or_default() += 1;
    }
    let mut out: Vec<GatedSnapshots> = counts
        .into_iter()
        .map(|((namespace, policy), count)| GatedSnapshots {
            namespace,
            policy,
            count,
        })
        .collect();
    out.sort_by(|a, b| (&a.namespace, &a.policy).cmp(&(&b.namespace, &b.policy)));
    out
}

/// `kind`/`namespace`/`name` attributes for the repository breaker gauges,
/// matching `kopiur_resource_phase` (a `ClusterRepository` gets `namespace=""`).
fn repo_health_attrs(kind: &'static str, ns: &str, name: &str) -> [KeyValue; 3] {
    [
        KeyValue::new("kind", kind),
        KeyValue::new("namespace", ns.to_string()),
        KeyValue::new("name", name.to_string()),
    ]
}

/// Value for `kopiur_repository_consecutive_backend_failures`: `Some` whenever
/// health status exists (the probe has run), defaulting an unset counter to 0 —
/// a recovered repository's explicit 0 is informative, while a repository whose
/// probe never ran is genuinely unknown and gets no series. Pure so the
/// emission rule is unit-tested off-OTel.
fn repo_consecutive_backend_failures(
    health: Option<&kopiur_api::repository::RepositoryHealthStatus>,
) -> Option<i64> {
    health.map(|h| h.consecutive_probe_failures.unwrap_or(0))
}

/// Value for `kopiur_repository_breaker_open_since_timestamp_seconds`: `Some`
/// (the `firstFailureAt` unix seconds) ONLY while the breaker is open — phase
/// `Degraded` AND `BackendReachable=False` — so the series exists exactly for
/// the open window and disappears on recovery. Pure so the open-window rule is
/// unit-tested off-OTel.
fn breaker_open_since(
    phase: Option<kopiur_api::RepositoryPhase>,
    health: Option<&kopiur_api::repository::RepositoryHealthStatus>,
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> Option<i64> {
    if phase != Some(kopiur_api::RepositoryPhase::Degraded) {
        return None;
    }
    let backend_unreachable = conditions
        .iter()
        .any(|c| c.type_ == crate::consts::BACKEND_REACHABLE_CONDITION && c.status == "False");
    if !backend_unreachable {
        return None;
    }
    health?
        .first_failure_at
        .as_deref()
        .and_then(parse_unix_seconds)
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
        repository_cr_with_status(ns, name, serde_json::json!({ "phase": phase }))
    }

    fn repository_cr_with_status(ns: &str, name: &str, status: serde_json::Value) -> Repository {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Repository",
            "metadata": { "name": name, "namespace": ns },
            "spec": {
                "backend": { "filesystem": { "path": "/repo" } },
                "encryption": { "passwordSecretRef": { "name": "s" } },
            },
            "status": status,
        }))
        .expect("valid Repository")
    }

    fn cluster_repository_cr(name: &str, phase: &str) -> ClusterRepository {
        cluster_repository_cr_with_status(name, serde_json::json!({ "phase": phase }))
    }

    fn cluster_repository_cr_with_status(
        name: &str,
        status: serde_json::Value,
    ) -> ClusterRepository {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "ClusterRepository",
            "metadata": { "name": name },
            "spec": {
                "backend": { "filesystem": { "path": "/repo" } },
                "encryption": { "passwordSecretRef": { "name": "s" } },
                "allowedNamespaces": { "all": true },
            },
            "status": status,
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
        m.inc_work_spec_cms_swept(1);
        m.inc_projected_secrets_swept(1);
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
        assert!(text.contains("kopiur_work_spec_cms_swept_total"), "{text}");
        assert!(
            text.contains("kopiur_projected_secrets_swept_total"),
            "{text}"
        );
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
    fn repo_catalog_sets_snapshot_discovered_and_foreign_gauges() {
        let m = Metrics::new();
        m.set_repo_catalog("apps", "nas", Some(42), Some(7), Some(3));
        let text = String::from_utf8(m.gather()).unwrap();
        let find = |metric: &str| -> String {
            text.lines()
                .find(|l| l.starts_with(&format!("{metric}{{")) && l.contains("name=\"nas\""))
                .unwrap_or_else(|| panic!("missing {metric}: {text}"))
                .to_string()
        };
        assert!(
            find("kopiur_repo_snapshot_count")
                .trim_end()
                .ends_with(" 42")
        );
        assert!(
            find("kopiur_repo_discovered_snapshots")
                .trim_end()
                .ends_with(" 7")
        );
        assert!(
            find("kopiur_repo_foreign_snapshots")
                .trim_end()
                .ends_with(" 3")
        );
    }

    #[test]
    fn repo_catalog_omits_the_foreign_gauge_series_when_unset() {
        // A `None` foreign count (e.g. a repository predating this metric, or one
        // with no cluster identity) must not fabricate a 0-valued series.
        let m = Metrics::new();
        m.set_repo_catalog("apps", "nas", Some(1), Some(0), None);
        let text = String::from_utf8(m.gather()).unwrap();
        assert!(
            !text
                .lines()
                .any(|l| l.starts_with("kopiur_repo_foreign_snapshots{")
                    && l.contains("name=\"nas\"")),
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

    /// Build a terminal Snapshot with an explicit creationTimestamp (issue #196
    /// orders "last backup" by creation, since a Failed run may carry no endTime).
    fn snap_created(name: &str, policy: &str, phase: &str, created: &str) -> Snapshot {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Snapshot",
            "metadata": { "name": name, "namespace": "apps", "creationTimestamp": created },
            "spec": { "policyRef": { "name": policy } },
            "status": { "phase": phase },
        }))
        .expect("valid Snapshot")
    }

    #[test]
    fn policy_backup_health_reflects_latest_terminal_attempt() {
        // A discovered Snapshot (no policyRef) is excluded even though it's newest.
        let discovered: Snapshot = serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Snapshot",
            "metadata": { "name": "disc", "namespace": "apps", "creationTimestamp": "2026-06-01T00:00:00Z" },
            "spec": {},
            "status": { "phase": "Discovered" },
        }))
        .expect("valid");
        let snaps: Vec<Arc<Snapshot>> = vec![
            // policy "a": older Succeeded, newer Failed → the last attempt failed → 0.
            snap_created("a-1", "a", "Succeeded", "2026-05-01T00:00:00Z"),
            snap_created("a-2", "a", "Failed", "2026-05-02T00:00:00Z"),
            // policy "b": older Failed, newer Succeeded → recovered → 1.
            snap_created("b-1", "b", "Failed", "2026-05-01T00:00:00Z"),
            snap_created("b-2", "b", "Succeeded", "2026-05-02T00:00:00Z"),
            // policy "c": only Succeeded → 1.
            snap_created("c-1", "c", "Succeeded", "2026-05-01T00:00:00Z"),
            // policy "d": only a Running (non-terminal) run → absent (unknown).
            snap_created("d-1", "d", "Running", "2026-05-01T00:00:00Z"),
        ]
        .into_iter()
        .chain(std::iter::once(discovered))
        .map(Arc::new)
        .collect();

        let health = policy_backup_health(&snaps);
        assert_eq!(
            health,
            vec![
                PolicyHealth {
                    namespace: "apps".into(),
                    policy: "a".into(),
                    healthy: false
                },
                PolicyHealth {
                    namespace: "apps".into(),
                    policy: "b".into(),
                    healthy: true
                },
                PolicyHealth {
                    namespace: "apps".into(),
                    policy: "c".into(),
                    healthy: true
                },
            ]
        );
        // The user's question: "how many of my policies are OK?" — a count of states,
        // NOT a run count (which would grow with every scheduled fire).
        assert_eq!(health.iter().filter(|h| h.healthy).count(), 2);
    }

    #[test]
    fn policy_backup_health_gauge_exports_zero_and_one_per_policy() {
        let m = Metrics::new();
        let snaps = make_store(vec![
            snap_created("ok-1", "ok", "Succeeded", "2026-05-01T00:00:00Z"),
            snap_created("bad-1", "bad", "Failed", "2026-05-01T00:00:00Z"),
        ]);
        let (_, repos, crepos, restores) = empty_stores();
        let text = gather_with(
            &m,
            ResourceStores {
                snapshots: snaps.0,
                repositories: repos.0,
                cluster_repositories: crepos.0,
                restores: restores.0,
            },
        );
        let line = |policy: &str| -> String {
            text.lines()
                .find(|l| {
                    l.starts_with("kopiur_snapshotpolicy_last_backup_success{")
                        && l.contains(&format!("policy=\"{policy}\""))
                })
                .unwrap_or_else(|| panic!("missing series for {policy}: {text}"))
                .rsplit(' ')
                .next()
                .unwrap()
                .to_string()
        };
        assert_eq!(line("ok"), "1");
        assert_eq!(line("bad"), "0");
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

    // --- mass-deletion protection plumbing (M3): outcome enums, gauge label
    // mapping, and the deletion-observability counting rule ---

    #[test]
    fn snapshot_deletion_outcome_labels_are_exhaustive_and_stable() {
        assert_eq!(SnapshotDeletionOutcome::Deleted.as_str(), "deleted");
        assert_eq!(SnapshotDeletionOutcome::Retained.as_str(), "retained");
        assert_eq!(SnapshotDeletionOutcome::Orphaned.as_str(), "orphaned");
        assert_eq!(
            SnapshotDeletionOutcome::CascadeRetained.as_str(),
            "cascade_retained"
        );
    }

    #[test]
    fn batch_job_and_member_outcome_labels_are_stable() {
        assert_eq!(BatchJobOutcome::Succeeded.as_str(), "succeeded");
        assert_eq!(BatchJobOutcome::Failed.as_str(), "failed");
        assert_eq!(BatchMemberOutcome::Deleted.as_str(), "deleted");
        assert_eq!(BatchMemberOutcome::Failed.as_str(), "failed");
    }

    #[test]
    fn repo_gauge_attrs_falls_back_to_the_unknown_bucket_when_unpinned() {
        use opentelemetry::Value;
        let attrs = repo_gauge_attrs(None);
        assert_eq!(attrs[0].value, Value::from("unknown"));
        assert_eq!(attrs[1].value, Value::from("unknown"));
    }

    #[test]
    fn repo_gauge_attrs_labels_by_the_pinned_repository_kind_and_name() {
        use opentelemetry::Value;
        let repo = RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "nas".into(),
            namespace: Some("team-a".into()),
        };
        let attrs = repo_gauge_attrs(Some(&repo));
        assert_eq!(attrs[0].value, Value::from("Repository"));
        assert_eq!(attrs[1].value, Value::from("nas".to_string()));

        let crepo = RepositoryRef {
            kind: RepositoryKind::ClusterRepository,
            name: "shared".into(),
            namespace: None,
        };
        let attrs = repo_gauge_attrs(Some(&crepo));
        assert_eq!(attrs[0].value, Value::from("ClusterRepository"));
        assert_eq!(attrs[1].value, Value::from("shared".to_string()));
    }

    /// A `Snapshot` mid-deletion: `deletionTimestamp` set, optionally carrying
    /// the cleanup finalizer and/or a `DeletionHeld` condition.
    fn deleting_snapshot(name: &str, with_finalizer: bool, held: bool) -> Snapshot {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

        let mut status = serde_json::json!({});
        if held {
            status["conditions"] = serde_json::json!([{
                "type": "DeletionHeld",
                "status": "True",
                "reason": "MassDeletionBreaker",
                "message": "held",
                "lastTransitionTime": "2026-01-01T00:00:00Z",
            }]);
        }
        let mut s = snapshot_cr("ns", name, None, status);
        s.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
        if with_finalizer {
            s.metadata.finalizers =
                Some(vec![crate::consts::SNAPSHOT_CLEANUP_FINALIZER.to_string()]);
        }
        s
    }

    #[test]
    fn counts_toward_deletion_gauge_requires_both_deletion_timestamp_and_finalizer() {
        assert!(counts_toward_deletion_gauge(&deleting_snapshot(
            "a", true, false
        )));
        // No deletionTimestamp: not deleting at all.
        let mut not_deleting = deleting_snapshot("b", true, false);
        not_deleting.metadata.deletion_timestamp = None;
        assert!(!counts_toward_deletion_gauge(&not_deleting));
        // Deleting but the finalizer already cleared: the deletion path is
        // done, nothing pending.
        assert!(!counts_toward_deletion_gauge(&deleting_snapshot(
            "c", false, false
        )));
    }

    #[test]
    fn deletion_held_reads_only_a_true_deletion_held_condition() {
        assert!(deletion_held(&deleting_snapshot("a", true, true)));
        assert!(!deletion_held(&deleting_snapshot("b", true, false)));
    }

    #[test]
    fn mass_deletion_metrics_register_and_export_under_kopiur_namespace() {
        let m = Metrics::new();
        m.inc_snapshot_deletion("ns", SnapshotDeletionOutcome::Deleted);
        m.inc_snapshot_cascade_retained("ns");
        m.inc_snapshot_policy_cascade_retained("ns");
        m.inc_snapshot_delete_batch_job(BatchJobOutcome::Succeeded);
        m.inc_snapshot_delete_batch_members(BatchMemberOutcome::Deleted, 3);

        let (_, repos, crepos, restores) = empty_stores();
        let snaps = make_store(vec![
            // Held: counts toward both the pending and the held gauge.
            deleting_snapshot("held-1", true, true),
            // Pending but not held: counts toward the pending gauge only.
            deleting_snapshot("pending-1", true, false),
        ]);
        let text = gather_with(
            &m,
            ResourceStores {
                snapshots: snaps.0,
                repositories: repos.0,
                cluster_repositories: crepos.0,
                restores: restores.0,
            },
        );
        assert!(text.contains("kopiur_snapshot_deletions_total"), "{text}");
        assert!(text.contains("outcome=\"deleted\""), "{text}");
        assert!(
            text.contains("kopiur_snapshots_cascade_retained_total"),
            "{text}"
        );
        assert!(
            text.contains("kopiur_snapshots_policy_cascade_retained_total"),
            "{text}"
        );
        assert!(
            text.contains("kopiur_snapshot_delete_batch_jobs_total"),
            "{text}"
        );
        assert!(
            text.contains("kopiur_snapshot_delete_batch_members_total"),
            "{text}"
        );
        assert!(
            text.contains("kopiur_snapshot_deletions_pending_external"),
            "{text}"
        );
        assert!(text.contains("kopiur_snapshot_deletions_held"), "{text}");
        // Unpinned Snapshots fall into the single unknown/unknown bucket.
        assert!(
            text.contains("repo_kind=\"unknown\"") && text.contains("repo_name=\"unknown\""),
            "{text}"
        );
    }

    // --- M6 (#345): migrated per-policy gauges + breaker observability ---

    /// Extract the value of the first series of `metric` whose labels contain
    /// every `contains` fragment; `None` when no such series exists.
    fn series_value(text: &str, metric: &str, contains: &[&str]) -> Option<String> {
        text.lines()
            .find(|l| {
                l.starts_with(&format!("{metric}{{")) && contains.iter().all(|c| l.contains(c))
            })
            .map(|l| l.rsplit(' ').next().unwrap().to_string())
    }

    /// The migrated `kopiur_snapshot_consecutive_failures` / `kopiur_snapshots_live`
    /// gauges derive from the Snapshot store per (namespace, policy) — labels stay
    /// {namespace, name} with name = the policy name — and their series disappear
    /// with the last Snapshot CR (the #172/#175 property the old sync gauges lacked).
    #[test]
    fn migrated_policy_gauges_derive_from_store_and_disappear() {
        let m = Metrics::new();
        let succeeded = snapshot_cr(
            "apps",
            "db-1",
            Some("daily"),
            succeeded_status(100, 10, "2026-05-24T00:00:00Z"),
        );
        let failed = snapshot_cr(
            "apps",
            "db-2",
            Some("daily"),
            serde_json::json!({
                "phase": "Failed",
                "timing": { "endTime": END_TIME },
            }),
        );
        // Non-terminal: counts toward live, never toward the streak.
        let running = snapshot_cr(
            "apps",
            "db-3",
            Some("daily"),
            serde_json::json!({ "phase": "Running" }),
        );
        // A discovered Snapshot (no policyRef) participates in NEITHER gauge.
        let discovered = snapshot_cr(
            "apps",
            "disc",
            None,
            serde_json::json!({ "phase": "Discovered" }),
        );

        let (reader, mut writer) = reflector::store::<Snapshot>();
        for cr in [&succeeded, &failed, &running, &discovered] {
            writer.apply_watcher_event(&watcher::Event::Apply(cr.clone()));
        }
        let (_, repos, crepos, restores) = empty_stores();
        m.register_resource_observers(ResourceStores {
            snapshots: reader,
            repositories: repos.0,
            cluster_repositories: crepos.0,
            restores: restores.0,
        });

        let text = String::from_utf8(m.gather()).unwrap();
        assert_eq!(
            series_value(
                &text,
                "kopiur_snapshot_consecutive_failures",
                &["namespace=\"apps\"", "name=\"daily\""],
            )
            .as_deref(),
            Some("1"),
            "trailing Failed after the Succeeded one: {text}"
        );
        assert_eq!(
            series_value(
                &text,
                "kopiur_snapshots_live",
                &["namespace=\"apps\"", "name=\"daily\""],
            )
            .as_deref(),
            Some("3"),
            "all three policyRef'd CRs count as live: {text}"
        );
        // The policy-less Snapshot minted no series of its own.
        assert!(
            series_value(&text, "kopiur_snapshots_live", &["name=\"disc\""]).is_none(),
            "{text}"
        );

        // Delete every Snapshot of the policy: both series vanish (no lingering
        // 0 — the defect class the sync gauges had).
        for cr in [succeeded, failed, running] {
            writer.apply_watcher_event(&watcher::Event::Delete(cr));
        }
        let after = String::from_utf8(m.gather()).unwrap();
        assert!(
            series_value(
                &after,
                "kopiur_snapshot_consecutive_failures",
                &["name=\"daily\""]
            )
            .is_none(),
            "{after}"
        );
        assert!(
            series_value(&after, "kopiur_snapshots_live", &["name=\"daily\""]).is_none(),
            "{after}"
        );
    }

    /// A Snapshot parked behind a not-Ready repository: Pending with the Ready
    /// condition carrying reason `RepositoryNotReady`.
    fn gated_snapshot(ns: &str, name: &str, policy: Option<&str>) -> Snapshot {
        snapshot_cr(
            ns,
            name,
            policy,
            serde_json::json!({
                "phase": "Pending",
                "conditions": [{
                    "type": "Ready",
                    "status": "False",
                    "reason": "RepositoryNotReady",
                    "message": "waiting for the repository",
                    "lastTransitionTime": "2026-01-01T00:00:00Z",
                }],
            }),
        )
    }

    #[test]
    fn gated_predicate_requires_pending_and_the_repository_not_ready_reason() {
        assert!(snapshot_gated(&gated_snapshot("apps", "a", Some("daily"))));
        // A Pending Snapshot held for a DIFFERENT reason (e.g. preflight) is not gated.
        let preflight = snapshot_cr(
            "apps",
            "b",
            None,
            serde_json::json!({
                "phase": "Pending",
                "conditions": [{
                    "type": "Ready",
                    "status": "False",
                    "reason": "PreflightFailed",
                    "message": "check failed",
                    "lastTransitionTime": "2026-01-01T00:00:00Z",
                }],
            }),
        );
        assert!(!snapshot_gated(&preflight));
        // A Running Snapshot with a stale RepositoryNotReady condition is not gated:
        // the phase gate keeps a launched backup out of the parked count.
        let mut launched = gated_snapshot("apps", "c", None);
        launched.status.as_mut().unwrap().phase = Some(SnapshotPhase::Running);
        assert!(!snapshot_gated(&launched));
        // No status at all: nothing to read, not gated.
        let mut bare = gated_snapshot("apps", "d", None);
        bare.status = None;
        assert!(!snapshot_gated(&bare));
    }

    #[test]
    fn gated_gauge_counts_per_policy_and_omits_the_policy_label_when_unset() {
        let m = Metrics::new();
        let g1 = gated_snapshot("apps", "g1", Some("daily"));
        let g2 = gated_snapshot("apps", "g2", Some("daily"));
        let adhoc = gated_snapshot("apps", "adhoc", None);
        let (reader, mut writer) = reflector::store::<Snapshot>();
        for cr in [&g1, &g2, &adhoc] {
            writer.apply_watcher_event(&watcher::Event::Apply(cr.clone()));
        }
        let (_, repos, crepos, restores) = empty_stores();
        m.register_resource_observers(ResourceStores {
            snapshots: reader,
            repositories: repos.0,
            cluster_repositories: crepos.0,
            restores: restores.0,
        });

        let text = String::from_utf8(m.gather()).unwrap();
        assert_eq!(
            series_value(&text, "kopiur_snapshot_gated", &["policy=\"daily\""]).as_deref(),
            Some("2"),
            "{text}"
        );
        // The policy-less parked Snapshot lands in a series WITHOUT a policy label.
        let no_policy_line = text
            .lines()
            .find(|l| {
                l.starts_with("kopiur_snapshot_gated{")
                    && l.contains("namespace=\"apps\"")
                    && !l.contains("policy=")
            })
            .unwrap_or_else(|| panic!("missing policy-less gated series: {text}"));
        assert!(
            !no_policy_line.trim_end().ends_with(" 2"),
            "{no_policy_line}"
        );
        assert!(
            no_policy_line.trim_end().ends_with(" 1"),
            "{no_policy_line}"
        );

        // Recovery: the repository came back and one parked Snapshot launched —
        // its updated (Running) copy leaves the gated population, and once every
        // parked CR is gone the series disappears entirely.
        let mut launched = g1.clone();
        launched.status.as_mut().unwrap().phase = Some(SnapshotPhase::Running);
        writer.apply_watcher_event(&watcher::Event::Apply(launched));
        let text = String::from_utf8(m.gather()).unwrap();
        assert_eq!(
            series_value(&text, "kopiur_snapshot_gated", &["policy=\"daily\""]).as_deref(),
            Some("1"),
            "{text}"
        );
        let mut launched2 = g2.clone();
        launched2.status.as_mut().unwrap().phase = Some(SnapshotPhase::Running);
        writer.apply_watcher_event(&watcher::Event::Apply(launched2));
        writer.apply_watcher_event(&watcher::Event::Delete(adhoc));
        let text = String::from_utf8(m.gather()).unwrap();
        assert!(
            !text
                .lines()
                .any(|l| l.starts_with("kopiur_snapshot_gated{")),
            "gated series must all disappear on recovery: {text}"
        );
    }

    /// Health status for a repository with a failing-probe streak, the loud
    /// condition, and the breaker open (phase supplied by the caller).
    fn unreachable_health_status(phase: &str, failures: i64) -> serde_json::Value {
        serde_json::json!({
            "phase": phase,
            "health": {
                "consecutiveProbeFailures": failures,
                "firstFailureAt": END_TIME,
            },
            "conditions": [{
                "type": "BackendReachable",
                "status": "False",
                "reason": "BackendUnreachable",
                "message": "connect failed",
                "lastTransitionTime": "2026-01-01T00:00:00Z",
            }],
        })
    }

    #[test]
    fn repository_backend_failure_gauges_cover_both_kinds_and_close_with_recovery() {
        let m = Metrics::new();
        let degraded =
            repository_cr_with_status("apps", "nas", unreachable_health_status("Degraded", 4));
        // Recovered ClusterRepository: health exists with the streak cleared —
        // the 0 IS emitted (informative), but no breaker-open series.
        let recovered = cluster_repository_cr_with_status(
            "shared",
            serde_json::json!({
                "phase": "Ready",
                "health": { "lastHealthyAt": END_TIME },
                "conditions": [{
                    "type": "BackendReachable",
                    "status": "True",
                    "reason": "Reachable",
                    "message": "ok",
                    "lastTransitionTime": "2026-01-01T00:00:00Z",
                }],
            }),
        );
        // Probe never ran: no health status, no series at all.
        let unprobed = repository_cr("apps", "cold", "Ready");

        let (repo_reader, mut repo_writer) = reflector::store::<Repository>();
        repo_writer.apply_watcher_event(&watcher::Event::Apply(degraded.clone()));
        repo_writer.apply_watcher_event(&watcher::Event::Apply(unprobed));
        let crepos = make_store(vec![recovered]);
        let (snaps, _, _, restores) = empty_stores();
        m.register_resource_observers(ResourceStores {
            snapshots: snaps.0,
            repositories: repo_reader,
            cluster_repositories: crepos.0,
            restores: restores.0,
        });

        let text = String::from_utf8(m.gather()).unwrap();
        assert_eq!(
            series_value(
                &text,
                "kopiur_repository_consecutive_backend_failures",
                &["kind=\"Repository\"", "namespace=\"apps\"", "name=\"nas\""],
            )
            .as_deref(),
            Some("4"),
            "{text}"
        );
        // Cluster-scoped: namespace="" like kopiur_resource_phase; recovered → 0.
        assert_eq!(
            series_value(
                &text,
                "kopiur_repository_consecutive_backend_failures",
                &[
                    "kind=\"ClusterRepository\"",
                    "namespace=\"\"",
                    "name=\"shared\""
                ],
            )
            .as_deref(),
            Some("0"),
            "{text}"
        );
        // No health status → no series (unknown, not a healthy 0).
        assert!(
            series_value(
                &text,
                "kopiur_repository_consecutive_backend_failures",
                &["name=\"cold\""],
            )
            .is_none(),
            "{text}"
        );
        // The breaker-open marker exists ONLY for the Degraded+unreachable repo,
        // valued at firstFailureAt.
        assert_eq!(
            series_value(
                &text,
                "kopiur_repository_breaker_open_since_timestamp_seconds",
                &["kind=\"Repository\"", "name=\"nas\""],
            )
            .as_deref(),
            Some(END_UNIX.to_string().as_str()),
            "{text}"
        );
        assert!(
            series_value(
                &text,
                "kopiur_repository_breaker_open_since_timestamp_seconds",
                &["name=\"shared\""],
            )
            .is_none(),
            "no breaker-open series for a closed breaker: {text}"
        );

        // The breaker closes (a connect succeeded): phase back to Ready, condition
        // True, streak 0 — the open-since series vanishes, the failures gauge drops
        // to an explicit 0.
        let healed = repository_cr_with_status(
            "apps",
            "nas",
            serde_json::json!({
                "phase": "Ready",
                "health": { "consecutiveProbeFailures": 0, "lastHealthyAt": END_TIME },
                "conditions": [{
                    "type": "BackendReachable",
                    "status": "True",
                    "reason": "Reachable",
                    "message": "ok",
                    "lastTransitionTime": "2026-01-02T00:00:00Z",
                }],
            }),
        );
        repo_writer.apply_watcher_event(&watcher::Event::Apply(healed));
        let after = String::from_utf8(m.gather()).unwrap();
        assert!(
            series_value(
                &after,
                "kopiur_repository_breaker_open_since_timestamp_seconds",
                &["name=\"nas\""],
            )
            .is_none(),
            "open-since series must disappear when the breaker closes: {after}"
        );
        assert_eq!(
            series_value(
                &after,
                "kopiur_repository_consecutive_backend_failures",
                &["name=\"nas\""],
            )
            .as_deref(),
            Some("0"),
            "{after}"
        );

        // And the CR's deletion removes the remaining series entirely.
        repo_writer.apply_watcher_event(&watcher::Event::Delete(degraded));
        let gone = String::from_utf8(m.gather()).unwrap();
        assert!(
            series_value(
                &gone,
                "kopiur_repository_consecutive_backend_failures",
                &["name=\"nas\""],
            )
            .is_none(),
            "{gone}"
        );
    }

    #[test]
    fn breaker_open_since_requires_degraded_phase_and_the_false_condition() {
        use kopiur_api::RepositoryPhase;
        let health: kopiur_api::repository::RepositoryHealthStatus = serde_json::from_value(
            serde_json::json!({ "consecutiveProbeFailures": 3, "firstFailureAt": END_TIME }),
        )
        .unwrap();
        let unreachable: Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> =
            serde_json::from_value(serde_json::json!([{
                "type": "BackendReachable",
                "status": "False",
                "reason": "BackendUnreachable",
                "message": "down",
                "lastTransitionTime": "2026-01-01T00:00:00Z",
            }]))
            .unwrap();

        // Open: Degraded + BackendReachable=False → firstFailureAt.
        assert_eq!(
            breaker_open_since(Some(RepositoryPhase::Degraded), Some(&health), &unreachable),
            Some(END_UNIX)
        );
        // Degraded for a NON-breaker reason (condition not False) → absent: a
        // retryable bootstrap failure is Degraded too, but it is not an open breaker.
        assert_eq!(
            breaker_open_since(Some(RepositoryPhase::Degraded), Some(&health), &[]),
            None
        );
        // Not Degraded → absent even with a stale False condition.
        assert_eq!(
            breaker_open_since(Some(RepositoryPhase::Ready), Some(&health), &unreachable),
            None
        );
        // Open but no firstFailureAt recorded → no fabricated timestamp.
        let no_first: kopiur_api::repository::RepositoryHealthStatus =
            serde_json::from_value(serde_json::json!({ "consecutiveProbeFailures": 3 })).unwrap();
        assert_eq!(
            breaker_open_since(
                Some(RepositoryPhase::Degraded),
                Some(&no_first),
                &unreachable
            ),
            None
        );
    }

    #[test]
    fn repo_consecutive_backend_failures_emits_iff_health_exists() {
        assert_eq!(repo_consecutive_backend_failures(None), None);
        let empty: kopiur_api::repository::RepositoryHealthStatus =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(repo_consecutive_backend_failures(Some(&empty)), Some(0));
        let failing: kopiur_api::repository::RepositoryHealthStatus =
            serde_json::from_value(serde_json::json!({ "consecutiveProbeFailures": 7 })).unwrap();
        assert_eq!(repo_consecutive_backend_failures(Some(&failing)), Some(7));
    }
}
