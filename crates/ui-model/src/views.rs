//! Read models: flattened, browser-shaped projections of the CRD status
//! surface, one type per screen or table row.
//!
//! ## Why phases are *views*
//!
//! Each `*PhaseView` mirrors a CR's `status.phase` but adds an
//! `Unknown { raw }` variant. The UI is versioned independently of the operator
//! it talks to, so a newer controller writing a phase this build has never
//! heard of must degrade to a labelled chip carrying the raw string — never to
//! a deserialization error that blanks the whole table.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::graph::{GateHit, Health};

/// View of `Repository`/`ClusterRepository` `status.phase`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum RepositoryPhaseView {
    /// Not reconciled yet.
    Pending,
    /// Connecting to or creating the kopia repository.
    Initializing,
    /// Connected and usable.
    Ready,
    /// Usable, but a health probe or maintenance run is failing.
    Degraded,
    /// Unusable.
    Failed,
    /// A phase this build does not recognize.
    Unknown {
        /// The phase string exactly as the operator wrote it.
        raw: String,
    },
}

/// View of `Snapshot` `status.phase`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum SnapshotPhaseView {
    /// Admitted, no mover Job yet.
    Pending,
    /// A mover Job is running.
    Running,
    /// The kopia snapshot exists.
    Succeeded,
    /// The run failed.
    Failed,
    /// The kopia snapshot is being deleted.
    Deleting,
    /// Adopted from a kopia manifest found in the repository rather than
    /// produced by this operator.
    Discovered,
    /// The source had no changes, so no new kopia snapshot was written.
    Unchanged,
    /// A phase this build does not recognize.
    Unknown {
        /// The phase string exactly as the operator wrote it.
        raw: String,
    },
}

/// View of `Restore` `status.phase`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum RestorePhaseView {
    /// Admitted, not started.
    Pending,
    /// Resolving the source snapshot and target claim.
    Resolving,
    /// A mover Job is writing files.
    Restoring,
    /// Files are on the target volume.
    Completed,
    /// The restore failed.
    Failed,
    /// A phase this build does not recognize.
    Unknown {
        /// The phase string exactly as the operator wrote it.
        raw: String,
    },
}

/// View of `RepositoryReplication`/`SnapshotReplication` `status.phase`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum ReplicationPhaseView {
    /// Admitted, waiting for its next scheduled run.
    Pending,
    /// A replication run is in flight.
    Replicating,
    /// The last run completed.
    Succeeded,
    /// The last run failed.
    Failed,
    /// Suspended by the user.
    Suspended,
    /// A phase this build does not recognize.
    Unknown {
        /// The phase string exactly as the operator wrote it.
        raw: String,
    },
}

/// View of `Snapshot` `status.origin` — how the snapshot came to exist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum OriginView {
    /// Fired by a `SnapshotSchedule`.
    Scheduled,
    /// Created directly by a user.
    Manual,
    /// Found in the repository by a catalog scan.
    Discovered,
    /// A discovered snapshot that was taken over by a policy.
    Adopted,
    /// Copied in by a `SnapshotReplication`.
    Replicated,
}

/// One page of a listing, carrying the window that produced it so the SPA can
/// render "showing 21-40 of 137" without a second request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Page<T> {
    /// The rows in this page.
    pub items: Vec<T>,
    /// How many rows exist across all pages.
    pub total: usize,
    /// Index of the first row in `items` within the full result set.
    pub offset: usize,
    /// Maximum rows the server was asked to return.
    pub limit: usize,
}

/// One row of the repositories table — `Repository` and `ClusterRepository`
/// projected into a single shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RepositorySummary {
    /// `Repository` or `ClusterRepository`.
    pub kind: String,
    /// `metadata.name`.
    pub name: String,
    /// `metadata.namespace`; absent for `ClusterRepository`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// `status.phase`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<RepositoryPhaseView>,
    /// Coarse health derived from phase and conditions.
    pub health: Health,
    /// Which `spec.backend` variant this repository uses (`s3`, `filesystem`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// How clients reach the repository — direct backend access or a kopia
    /// repository server.
    pub mode: String,
    /// `spec.suspend`.
    pub suspended: bool,
    /// `status.stats.snapshotCount`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_count: Option<i64>,
    /// `status.stats.totalSizeBytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_size_bytes: Option<i64>,
    /// `status.stats.indexBlobCount`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_blob_count: Option<i64>,
    /// RFC3339 timestamp of the last successful stats observation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observed_at: Option<String>,
    /// `status.server.endpoint`, when running in repository-server mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_endpoint: Option<String>,
    /// For `ClusterRepository`: how many namespaces `spec.allowedNamespaces`
    /// currently admits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_namespace_count: Option<i64>,
}

/// Everything the repository detail screen shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RepositoryDetail {
    /// The same fields the table row shows.
    pub summary: RepositorySummary,
    /// `status.resolved.identity.cluster` — the cluster identity component the
    /// operator resolved for snapshots written here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_cluster: Option<String>,
    /// Catalog-scan results, when catalog discovery is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<CatalogView>,
    /// Health-probe results, when probing is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthProbeView>,
    /// Repository-server state, when running in server mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerView>,
    /// Seed/bootstrap state, when this repository was seeded from another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<SeedView>,
    /// The `Maintenance` resource governing this repository, managed or
    /// externally authored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<MaintenanceRow>,
    /// Gates currently holding this repository back.
    pub gates: Vec<GateHit>,
    /// `status.conditions`.
    pub conditions: Vec<ConditionView>,
    /// Policies that write into this repository.
    pub policies: Vec<PolicyRef>,
    /// Names of replications that read from this repository.
    pub replications_out: Vec<String>,
    /// Names of replications that write into this repository.
    pub replications_in: Vec<String>,
    /// Browse sessions currently open against this repository.
    pub sessions: Vec<SessionInfo>,
}

/// `Repository.status.catalog` — what a catalog scan found in the repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CatalogView {
    /// Kopia snapshots discovered and adopted as `Snapshot` resources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovered_backup_count: Option<i64>,
    /// Kopia snapshots present in the repository that no kopiur identity claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreign_snapshot_count: Option<i64>,
    /// RFC3339 timestamp of the last catalog scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh_at: Option<String>,
}

/// `Repository.status.health` — the periodic connectivity probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct HealthProbeView {
    /// RFC3339 timestamp of the most recent probe, successful or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probe_at: Option<String>,
    /// RFC3339 timestamp of the most recent successful probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_healthy_at: Option<String>,
    /// Probe failures since the last success; drives the `Degraded` health.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_probe_failures: Option<i64>,
}

/// `Repository.status.server` — the kopia repository server, when enabled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ServerView {
    /// Address clients connect to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Whether the server admits writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    /// How the server authenticates clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,
}

/// `Repository.status.seed` — how this repository was bootstrapped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SeedView {
    /// Which seeding strategy ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The repository that was seeded from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// RFC3339 timestamp of when seeding completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seeded_at: Option<String>,
    /// How many snapshots the seed copied across.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots_copied: Option<i64>,
}

/// One `status.conditions[]` entry, flattened for display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ConditionView {
    /// `conditions[].type`, e.g. `Ready`.
    pub r#type: String,
    /// `conditions[].status` — `True`, `False`, or `Unknown`.
    pub status: String,
    /// `conditions[].reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `conditions[].message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// `conditions[].lastTransitionTime` as RFC3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<String>,
}

/// A namespaced reference to a `SnapshotPolicy`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PolicyRef {
    /// `metadata.namespace` of the policy.
    pub namespace: String,
    /// `metadata.name` of the policy.
    pub name: String,
}

/// One row of the snapshots table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SnapshotRow {
    /// `metadata.namespace`.
    pub namespace: String,
    /// `metadata.name`.
    pub name: String,
    /// `status.phase`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<SnapshotPhaseView>,
    /// `status.origin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<OriginView>,
    /// Name of the `SnapshotPolicy` this snapshot was taken under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// Name of the repository the snapshot lives in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// `status.kopiaSnapshotId` — the kopia manifest ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kopia_snapshot_id: Option<String>,
    /// The resolved kopia identity (`user@host:/path`) the snapshot was written
    /// under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// `status.startTime` as RFC3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    /// `status.endTime` as RFC3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    /// Logical size of the snapshotted tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<i64>,
    /// Bytes this snapshot actually added to the repository after dedup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_new: Option<i64>,
    /// Files considered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_total: Option<i64>,
    /// Files kopia could not read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_failed: Option<i64>,
    /// Whether the kopia manifest carries a retention pin, exempting it from
    /// GFS pruning.
    pub pinned: bool,
    /// `spec.deletionPolicy` — `Delete`, `Retain`, or `Orphan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_policy: Option<String>,
    /// For replicated snapshots: the repository the copy came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copied_from: Option<String>,
}

/// Everything the snapshot detail screen shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SnapshotDetail {
    /// The same fields the table row shows.
    pub row: SnapshotRow,
    /// Full kopia upload counters, when the run recorded them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<SnapshotStatsView>,
    /// Wall-clock duration between `startTime` and `endTime`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<i64>,
    /// The source paths the snapshot covered.
    pub sources: Vec<String>,
    /// Where this snapshot came from and where it has been copied to.
    pub lineage: Lineage,
    /// Whether the governing retention policy would keep this snapshot today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_preview: Option<RetentionPreview>,
    /// Failure detail, present when the run failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureView>,
    /// Last lines of the mover Job's log.
    pub log_tail: Vec<String>,
    /// `status.conditions`.
    pub conditions: Vec<ConditionView>,
    /// Gates currently holding this snapshot back.
    pub gates: Vec<GateHit>,
    /// Whether the UI may open a browse session against this snapshot.
    pub browsable: bool,
    /// Why browsing is unavailable, when `browsable` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browse_blocker: Option<String>,
}

/// `Snapshot.status.stats` — kopia's upload counters for one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SnapshotStatsView {
    /// Logical size of the snapshotted tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<i64>,
    /// Bytes added to the repository after dedup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_new: Option<i64>,
    /// Files seen for the first time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_new: Option<i64>,
    /// Files whose contents changed since the previous snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_modified: Option<i64>,
    /// Files reused unchanged from the previous snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_unchanged: Option<i64>,
    /// Files kopia could not read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_failed: Option<i64>,
}

/// Where a snapshot came from and where it has been copied to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Lineage {
    /// The repository this snapshot was replicated in from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copied_from_repository: Option<String>,
    /// The kopia manifest ID in the source repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_manifest_id: Option<String>,
    /// `Snapshot` resources that are copies of this one.
    pub copies: Vec<SnapshotRefView>,
}

/// A namespaced reference to a `Snapshot`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SnapshotRefView {
    /// `metadata.namespace` of the snapshot.
    pub namespace: String,
    /// `metadata.name` of the snapshot.
    pub name: String,
}

/// Whether the governing GFS retention would keep this snapshot right now, and
/// which rules say so.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RetentionPreview {
    /// True when at least one retention rule keeps the snapshot.
    pub kept: bool,
    /// The rules that matched, e.g. `keepDaily slot 3`.
    pub reasons: Vec<String>,
    /// RFC3339 timestamp the preview was computed at — the answer moves as time
    /// passes, so the UI shows when it was true.
    pub computed_at: String,
}

/// Why a run failed, in the terms the operator's own error classification uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct FailureView {
    /// The classified kopia error, e.g. `RepositoryUnreachable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kopia_error_class: Option<String>,
    /// The failure message shown to the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Exit code of the mover process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether the operator considers a retry likely to succeed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_recommended: Option<bool>,
    /// The mover operation that failed, e.g. `snapshot`, `restore`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
}

/// One row of the policies table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PolicyRow {
    /// `metadata.namespace`.
    pub namespace: String,
    /// `metadata.name`.
    pub name: String,
    /// Repositories this policy writes into.
    pub repositories: Vec<String>,
    /// True when the policy fans out to more than one repository.
    pub multi_repo: bool,
    /// `spec.suspend`.
    pub suspended: bool,
    /// RFC3339 timestamp of the most recent successful snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_snapshot: Option<String>,
    /// RFC3339 timestamp of the most recent successful verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified: Option<String>,
    /// How many non-deleted `Snapshot` resources this policy currently owns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_snapshot_count: Option<i64>,
}

/// Everything the policy detail screen shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PolicyDetail {
    /// The same fields the table row shows.
    pub row: PolicyRow,
    /// The resolved kopia identity (`user@host:/path`) snapshots are written
    /// under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// The source paths the policy backs up.
    pub sources: Vec<String>,
    /// `spec.retention` — the GFS rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RetentionView>,
    /// Per-repository verification state.
    pub verification: Vec<RepoVerificationView>,
    /// Schedules that fire this policy.
    pub schedules: Vec<ScheduleRow>,
    /// The most recent snapshots this policy produced.
    pub recent_snapshots: Vec<SnapshotRow>,
    /// Gates currently holding this policy back.
    pub gates: Vec<GateHit>,
    /// `status.conditions`.
    pub conditions: Vec<ConditionView>,
}

/// `SnapshotPolicy.spec.retention` — the GFS rules, `None` meaning unset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RetentionView {
    /// Most recent snapshots to keep regardless of age.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_latest: Option<i32>,
    /// Hourly slots to keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_hourly: Option<i32>,
    /// Daily slots to keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_daily: Option<i32>,
    /// Weekly slots to keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_weekly: Option<i32>,
    /// Monthly slots to keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_monthly: Option<i32>,
    /// Annual slots to keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_annual: Option<i32>,
}

/// Verification state for one repository a policy writes into.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RepoVerificationView {
    /// Name of the repository.
    pub repository: String,
    /// RFC3339 timestamp of the last successful verification there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified: Option<String>,
}

/// One row of the schedules table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ScheduleRow {
    /// `metadata.namespace`.
    pub namespace: String,
    /// `metadata.name`.
    pub name: String,
    /// The single policy this schedule fires, when it names one directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// The label selector this schedule fires by, when it selects policies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_selector: Option<String>,
    /// `spec.schedule.cron`, including any `H` jitter token as written.
    pub cron: String,
    /// `spec.schedule.timezone`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// `spec.suspend`.
    pub suspended: bool,
    /// RFC3339 timestamp of the last fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fire: Option<String>,
    /// RFC3339 timestamp of the next computed fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire: Option<String>,
    /// Name of the `Snapshot` the last fire produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_snapshot: Option<String>,
    /// Failures since the last success; bounded by `failedJobsHistoryLimit`.
    pub consecutive_failures: i64,
}

/// One row of the restores table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RestoreRow {
    /// `metadata.namespace`.
    pub namespace: String,
    /// `metadata.name`.
    pub name: String,
    /// `status.phase`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<RestorePhaseView>,
    /// Which `spec.source` variant this restore uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    /// Which `spec.target` variant this restore uses.
    pub target_kind: String,
    /// Repository the snapshot is read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// The resolved kopia manifest ID being restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kopia_snapshot_id: Option<String>,
    /// `status.startTime` as RFC3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    /// `status.endTime` as RFC3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    /// Bytes written to the target so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_restored: Option<i64>,
    /// Files written to the target so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_restored: Option<i64>,
    /// Per-claim progress, one entry per target PVC.
    pub claims: Vec<RestoreClaimView>,
}

/// Per-PVC progress within one restore.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RestoreClaimView {
    /// Name of the target `PersistentVolumeClaim`.
    pub pvc: String,
    /// The claim's restore phase.
    pub phase: String,
    /// Detail for this claim, typically a failure reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// One row of the maintenance table — a `Maintenance` resource's two run
/// tracks plus any pending manual run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct MaintenanceRow {
    /// `metadata.namespace`.
    pub namespace: String,
    /// `metadata.name`.
    pub name: String,
    /// Repository this maintenance governs.
    pub repository: String,
    /// The owning repository, when this `Maintenance` was projected from a
    /// repository's `spec.maintenance`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// True when the operator owns this `Maintenance`; false when a user
    /// authored it, in which case the operator honors it but never rewrites it.
    pub managed_by_repository: bool,
    /// The quick-maintenance track.
    pub quick: RunStatusView,
    /// The full-maintenance track.
    pub full: RunStatusView,
    /// A manual run the user requested, if one is pending or recent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_run: Option<ManualRunView>,
}

/// One maintenance track's run history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RunStatusView {
    /// RFC3339 timestamp of the last run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    /// RFC3339 timestamp of the next computed run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_scheduled_at: Option<String>,
    /// Failures since the last success.
    pub consecutive_failures: i64,
    /// Bytes the last run reclaimed by dropping unreachable content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_content_reclaimed_bytes: Option<i64>,
}

/// A user-requested maintenance run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ManualRunView {
    /// RFC3339 timestamp of when the run was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_at: Option<String>,
    /// `quick` or `full`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The run's current phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// RFC3339 timestamp of when the run finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

/// One row of the repository-replications table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RepositoryReplicationRow {
    /// `metadata.namespace`.
    pub namespace: String,
    /// `metadata.name`.
    pub name: String,
    /// Repository whose blobs are copied.
    pub source: String,
    /// Which backend variant the copy is written to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_backend: Option<String>,
    /// `spec.schedule.cron`.
    pub cron: String,
    /// `spec.suspend`.
    pub suspended: bool,
    /// `status.phase`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ReplicationPhaseView>,
    /// RFC3339 timestamp of the last successful replication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_replicated: Option<String>,
    /// RFC3339 timestamp of the next computed run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_scheduled_at: Option<String>,
    /// Bytes the last run copied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_replicated_bytes: Option<i64>,
    /// Blobs the last run copied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_replicated_blobs: Option<i64>,
}

/// One row of the snapshot-replications table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SnapshotReplicationRow {
    /// `metadata.namespace`.
    pub namespace: String,
    /// `metadata.name`.
    pub name: String,
    /// Repository snapshots are read from.
    pub source: String,
    /// Repository snapshots are copied into.
    pub destination: String,
    /// `spec.schedule.cron`.
    pub cron: String,
    /// `spec.suspend`.
    pub suspended: bool,
    /// `status.phase`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ReplicationPhaseView>,
    /// RFC3339 timestamp of the last successful replication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_replicated: Option<String>,
    /// Kopia identities the last run's selector matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identities_selected: Option<u32>,
    /// Snapshots the last run copied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots_copied: Option<u32>,
    /// Snapshots the last run skipped because the destination already had them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub already_present: Option<u32>,
    /// Snapshots the last run failed to copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<u32>,
    /// Snapshots the last run pruned from the destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pruned: Option<u32>,
}

/// One check from the doctor report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DoctorCheckView {
    /// Stable check identifier, e.g. `crds-installed`.
    pub check: String,
    /// Human-readable name of the check.
    pub title: String,
    /// `Pass`, `Warn`, or `Fail`.
    pub outcome: String,
    /// What the check found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub what: Option<String>,
    /// Why that is a problem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// What to do about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// The full doctor report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DoctorReportView {
    /// Every check that ran, in report order.
    pub checks: Vec<DoctorCheckView>,
    /// The exit code `kubectl kopiur doctor` would have returned — `0` all
    /// pass, `1` warnings, `2` failures.
    pub exit_code: u8,
    /// RFC3339 timestamp of when the report was produced.
    pub ran_at: String,
}

/// Static documentation for one gate the operator can apply, so the UI can
/// explain a gate it has never seen fire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct GateDescriptor {
    /// The CRD kind this gate applies to.
    pub scope: String,
    /// The `status.conditions[].type` that carries the gate.
    pub condition: String,
    /// The `status` value that means "blocked" for this condition — `True` for
    /// some gates, `False` for others.
    pub blocked_status: String,
    /// The `reason` this gate writes.
    pub reason: String,
    /// How serious the gate is — `info`, `warning`, or `error`.
    pub severity: String,
}

/// One Kubernetes `Event`, projected for display next to the object it
/// concerns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct EventRow {
    /// RFC3339 timestamp of the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    /// `Normal` or `Warning`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// The event's machine-readable reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The event's human-readable message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The object the event concerns, as `Kind/namespace/name`.
    pub regarding: String,
}

/// What a directory entry is, inside a browse session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link.
    Symlink,
    /// Anything else kopia reported — a socket, a device node, or a type this
    /// build does not recognize.
    Other {
        /// The entry type exactly as kopia reported it.
        raw: String,
    },
}

/// One entry in a browsed snapshot directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DirEntryView {
    /// The entry's basename.
    pub name: String,
    /// What the entry is.
    pub kind: EntryKind,
    /// Size in bytes, for files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    /// RFC3339 modification time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<String>,
    /// Unix mode as kopia rendered it, e.g. `drwxr-xr-x`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// One page of a browsed directory, plus the session that served it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DirListing {
    /// The directory path inside the snapshot.
    pub path: String,
    /// The entries in this page.
    pub entries: Vec<DirEntryView>,
    /// How many entries the directory holds in total.
    pub total: usize,
    /// Index of the first entry in `entries` within the directory.
    pub offset: usize,
    /// Maximum entries the server was asked to return.
    pub limit: usize,
    /// The browse session this listing came from — the SPA reuses it for the
    /// next navigation instead of paying for a new mover pod.
    pub session: SessionInfo,
}

/// A browse session: a long-lived mover Job the UI execs into to read a
/// snapshot's contents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SessionInfo {
    /// Namespace the session's Job runs in.
    pub namespace: String,
    /// Name of the session's Job.
    pub job: String,
    /// Name of the Job's pod, once scheduled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<String>,
    /// True when an existing session was reused rather than a new one started.
    pub reused: bool,
    /// RFC3339 timestamp of when the session will be reaped if left idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// What a mutating action created, returned so the SPA can navigate straight to
/// the new object instead of polling a list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ActionReceipt {
    /// Which action ran, e.g. `snapshotNow`.
    pub kind: String,
    /// The resources the action created.
    pub created: Vec<SnapshotRefView>,
    /// RFC3339 timestamp of when the action was accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_at: Option<String>,
    /// Anything the user should know about the outcome — e.g. that a fan-out
    /// created fewer snapshots than repositories because some were suspended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The dashboard's top-level status, wrapping the `kopiur-ops` status report
/// verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct StatusOverview {
    /// The `kopiur_ops::status::StatusReport` as JSON, passed straight through.
    ///
    /// Deliberately opaque on the wire (`unknown` in TypeScript): the report is
    /// the CLI's own shape and evolves with it, so pinning a second copy of it
    /// here would guarantee drift. The SPA narrows it at the point of use.
    #[ts(type = "unknown")]
    pub report: serde_json::Value,
    /// RFC3339 timestamp of when the server assembled the report, so relative
    /// times ("2 minutes ago") are computed against the server's clock.
    pub now: String,
}
