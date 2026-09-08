//! Cluster status roll-up: the cross-CRD join behind `kubectl kopiur status`.
//!
//! Split in two on purpose. [`gather`] is pure kube IO — seven `list` calls,
//! nothing interpreted. [`build_report`] is a pure function from those typed
//! objects to a [`StatusReport`]: it holds every filter and every roll-up rule,
//! and it is testable without a cluster. Rendering (a table, a web page) is the
//! caller's.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kopiur_api::common::{PhaseLabel, RepositoryKind};
use kopiur_api::consts::{MAINTENANCE_CONFIGURED_CONDITION, READY_CONDITION, STALLED_CONDITION};
use kopiur_api::{
    ClusterRepository, Repository, Restore, RestorePhase, Snapshot, SnapshotPhase, SnapshotPolicy,
    SnapshotReplication, SnapshotReplicationRunStats, SnapshotSchedule,
};
use kube::ResourceExt;
use kube::api::{Api, ListParams};
use serde::Serialize;

use crate::ctx::{OpsCtx, Scope};
use crate::error::{OpsError, classify_kube};
use crate::format::EMPTY_CELL;
use crate::snapshots::{RepoFilter, matches_repository};

/// The typed report `-o yaml|json` emits (and the table renders).
#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StatusReport {
    /// Repositories (namespaced + cluster-scoped), with readiness detail.
    pub repositories: Vec<RepoRow>,
    /// SnapshotPolicies with their last snapshot / verification.
    pub policies: Vec<PolicyRow>,
    /// SnapshotSchedules with firing detail.
    pub schedules: Vec<ScheduleRow>,
    /// SnapshotReplications with their last run's counters.
    pub snapshot_replications: Vec<SnapshotReplicationRow>,
    /// Snapshots/Restores currently in a non-terminal phase.
    pub in_flight: InFlight,
    /// Objects reporting the kstatus `Stalled=True` condition.
    pub stalled: Vec<StalledRow>,
}

/// One repository line.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoRow {
    /// `Repository` or `ClusterRepository`.
    pub kind: String,
    /// Object name.
    pub name: String,
    /// Namespace; absent for ClusterRepository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// `status.phase` as reported.
    pub phase: String,
    /// Backend discriminant (`S3`, `Filesystem`, …).
    pub backend: String,
    /// `spec.mode` (ReadWrite/ReadOnly).
    pub mode: String,
    /// `spec.suspend`.
    pub suspended: bool,
    /// Whether a Maintenance covers it (`MaintenanceConfigured` condition).
    pub maintenance: String,
    /// `spec.identityDefaults.cluster` — this cluster's identity suffix when
    /// the repository is shared across clusters; absent when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    /// `status.catalog.foreignSnapshotCount` — snapshots the last catalog scan
    /// classified as another cluster's; absent when never scanned or the
    /// repository has no `cluster` identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreign_snapshots: Option<i64>,
    /// `status.catalog.discoveredBackupCount` — how many `Snapshot` CRs the
    /// last catalog scan materialized from history already in the repository;
    /// absent when the repository has never been scanned. Plain inventory: a
    /// non-zero count is normal for a shared or re-seeded repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovered: Option<i64>,
    /// The `Ready` condition message when the repo is NOT Ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// One policy line.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyRow {
    /// Object name.
    pub name: String,
    /// Namespace.
    pub namespace: String,
    /// The referenced repository (`kind/name`).
    pub repository: String,
    /// `spec.suspend`.
    pub suspended: bool,
    /// `status.lastSuccessfulSnapshot` (RFC3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_snapshot: Option<String>,
    /// `status.lastVerified` (RFC3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_verified: Option<String>,
}

/// One schedule line.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRow {
    /// Object name.
    pub name: String,
    /// Namespace.
    pub namespace: String,
    /// `policyRef.name` or `selector` for policySelector fan-out.
    pub policy: String,
    /// The cron expression.
    pub cron: String,
    /// `spec.schedule.suspend`.
    pub suspended: bool,
    /// `status.lastSchedule.at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fire: Option<String>,
    /// `status.nextSchedule.at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_fire: Option<String>,
    /// `status.consecutiveFailures` when non-zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consecutive_failures: Option<i64>,
}

/// One snapshot-replication line — the named consumer of
/// `SnapshotReplication.status.lastRun` (the wiring ratchet's contract for
/// those counters).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReplicationRow {
    /// Object name.
    pub name: String,
    /// Namespace.
    pub namespace: String,
    /// The source repository (`kind/name`).
    pub source: String,
    /// The destination repository (`kind/name`).
    pub destination: String,
    /// `status.phase` as reported.
    pub phase: String,
    /// `spec.suspend`.
    pub suspended: bool,
    /// `status.lastReplicated` (RFC3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_replicated: Option<String>,
    /// `status.lastRun` counters (identitiesSelected / snapshotsCopied /
    /// alreadyPresent / failed / pruned).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<SnapshotReplicationRunStats>,
    /// The `Ready` condition message when the replication is NOT Ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// Counts of non-terminal work.
#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InFlight {
    /// Snapshots in Pending/Running.
    pub snapshots: usize,
    /// Restores in Pending/Resolving/Restoring.
    pub restores: usize,
}

/// One stalled object (kstatus `Stalled=True`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StalledRow {
    /// Kind of the stalled object.
    pub kind: String,
    /// `namespace/name`.
    pub object: String,
    /// The Stalled condition's message.
    pub message: String,
}

/// Everything one `status` pass reads from the cluster, before any
/// interpretation. `snapshot_replications` is `None` when the CRD is not
/// installed — distinct from an installed-but-empty `Some(vec![])`.
#[derive(Debug, Default)]
pub struct StatusInputs {
    /// Namespaced Repositories in scope.
    pub repositories: Vec<Repository>,
    /// ClusterRepositories (always cluster-scoped).
    pub cluster_repositories: Vec<ClusterRepository>,
    /// SnapshotPolicies in scope.
    pub policies: Vec<SnapshotPolicy>,
    /// SnapshotSchedules in scope.
    pub schedules: Vec<SnapshotSchedule>,
    /// SnapshotReplications in scope; `None` when the CRD is absent.
    pub snapshot_replications: Option<Vec<SnapshotReplication>>,
    /// Snapshots in scope.
    pub snapshots: Vec<Snapshot>,
    /// Restores in scope.
    pub restores: Vec<Restore>,
}

fn condition<'a>(conditions: &'a [Condition], type_: &str) -> Option<&'a Condition> {
    conditions.iter().find(|c| c.type_ == type_)
}

/// Is a Snapshot non-terminal? Exhaustive.
fn snapshot_in_flight(phase: Option<&SnapshotPhase>) -> bool {
    match phase {
        Some(SnapshotPhase::Pending | SnapshotPhase::Running) | None => true,
        Some(
            SnapshotPhase::Succeeded
            | SnapshotPhase::Failed
            | SnapshotPhase::Deleting
            | SnapshotPhase::Discovered
            | SnapshotPhase::Unchanged,
        ) => false,
        // Counted as in-flight: an unrecognized phase is never reported as
        // finished work, so `kubectl kopiur status` surfaces it instead of
        // quietly dropping it from every total.
        Some(SnapshotPhase::Unknown(_)) => true,
    }
}

/// Is a Restore non-terminal? Exhaustive.
fn restore_in_flight(phase: Option<&RestorePhase>) -> bool {
    match phase {
        Some(RestorePhase::Pending | RestorePhase::Resolving | RestorePhase::Restoring) | None => {
            true
        }
        Some(RestorePhase::Completed | RestorePhase::Failed) => false,
        // Counted as in-flight — see `snapshot_in_flight`.
        Some(RestorePhase::Unknown(_)) => true,
    }
}

/// Does this repository row match the resolved `--repository` filter?
/// Namespace-aware: two Repositories named `nas` in different namespaces are
/// different repositories.
fn repo_matches(
    filter: Option<&RepoFilter>,
    kind: RepositoryKind,
    name: &str,
    namespace: Option<&str>,
) -> bool {
    match filter {
        None => true,
        Some(f) => {
            f.kind == kind
                && f.name == name
                && match kind {
                    RepositoryKind::ClusterRepository => true,
                    RepositoryKind::Repository => f.namespace.as_deref() == namespace,
                }
        }
    }
}

/// Render a policy's repository target for the status table. A display path
/// has no error surface, so the invalid/multi shapes get honest placeholders
/// instead of a silent repository #1 — the per-repository detail for a
/// multi-repo policy lives in `kubectl get snapshotpolicy` (the
/// `Repositories` print column renders `status.repositorySummary`).
fn policy_repository_display(spec: &kopiur_api::SnapshotPolicySpec) -> String {
    use kopiur_api::PolicyRepositories;
    match kopiur_api::policy_repositories(spec) {
        Ok(PolicyRepositories::Single(r)) => format!("{:?}/{}", r.kind, r.name),
        Ok(PolicyRepositories::Multi(refs)) => format!("(multiple: {})", refs.len()),
        Err(_) => "(invalid)".to_string(),
    }
}

/// Does ANY of a policy's repository refs (`spec.repository` /
/// `spec.repositories`) match the filter? An absent ref namespace means "same
/// as the policy" for a namespaced Repository. Any-of, so a multi-repo policy
/// shows up under every repository it targets.
fn policy_matches(filter: Option<&RepoFilter>, policy: &SnapshotPolicy) -> bool {
    let Some(f) = filter else { return true };
    kopiur_api::repository_refs(&policy.spec).any(|rref| {
        if rref.kind != f.kind || rref.name != f.name {
            return false;
        }
        match f.kind {
            RepositoryKind::ClusterRepository => true,
            RepositoryKind::Repository => {
                let effective = rref
                    .namespace
                    .as_deref()
                    .or(policy.metadata.namespace.as_deref());
                effective == f.namespace.as_deref()
            }
        }
    })
}

/// Does a `SnapshotReplication` touch the filtered repository — as its source
/// OR its destination? An absent ref namespace means "same as the CR" for a
/// namespaced Repository.
fn replication_matches(filter: Option<&RepoFilter>, r: &SnapshotReplication) -> bool {
    let Some(f) = filter else { return true };
    let cr_ns = r.metadata.namespace.as_deref();
    let ref_matches = |rref: &kopiur_api::common::RepositoryRef| {
        rref.kind == f.kind
            && rref.name == f.name
            && match f.kind {
                RepositoryKind::ClusterRepository => true,
                RepositoryKind::Repository => {
                    rref.namespace.as_deref().or(cr_ns) == f.namespace.as_deref()
                }
            }
    };
    ref_matches(&r.spec.source_ref) || ref_matches(&r.spec.destination_ref)
}

/// Build the snapshot-replication row from a typed object. Pure.
fn snapshot_replication_row(r: &SnapshotReplication) -> SnapshotReplicationRow {
    let status = r.status.as_ref();
    let conditions = status.map(|s| s.conditions.as_slice()).unwrap_or_default();
    SnapshotReplicationRow {
        name: r.name_any(),
        namespace: r.metadata.namespace.clone().unwrap_or_default(),
        source: format!("{:?}/{}", r.spec.source_ref.kind, r.spec.source_ref.name),
        destination: format!(
            "{:?}/{}",
            r.spec.destination_ref.kind, r.spec.destination_ref.name
        ),
        phase: status
            .and_then(|s| s.phase.as_ref())
            .map(|p| p.label().to_string())
            .unwrap_or_else(|| EMPTY_CELL.into()),
        suspended: r.spec.suspend,
        last_replicated: status.and_then(|s| s.last_replicated.clone()),
        last_run: status.and_then(|s| s.last_run),
        problem: condition(conditions, READY_CONDITION)
            .filter(|c| c.status != "True")
            .map(|c| c.message.clone()),
    }
}

/// Does a Restore belong to the filtered repository? Matches the pinned
/// `status.resolved.repository`, the explicit `spec.repository`, or — for a
/// fromPolicy source — a kept policy `(namespace, name)`.
fn restore_matches(
    filter: Option<&RepoFilter>,
    kept_policies: &std::collections::BTreeSet<(String, String)>,
    restore: &Restore,
) -> bool {
    let Some(f) = filter else { return true };
    let restore_ns = restore.metadata.namespace.as_deref();
    let ref_matches = |rref: &kopiur_api::common::RepositoryRef| {
        rref.kind == f.kind
            && rref.name == f.name
            && match f.kind {
                RepositoryKind::ClusterRepository => true,
                RepositoryKind::Repository => {
                    rref.namespace.as_deref().or(restore_ns) == f.namespace.as_deref()
                }
            }
    };
    if let Some(rref) = restore
        .status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .and_then(|r| r.repository.as_ref())
        && ref_matches(rref)
    {
        return true;
    }
    if let Some(rref) = restore.spec.repository.as_ref()
        && ref_matches(rref)
    {
        return true;
    }
    if let kopiur_api::RestoreSource::FromPolicy(c) = &restore.spec.source {
        let policy_ns = c
            .namespace
            .clone()
            .or_else(|| restore.metadata.namespace.clone())
            .unwrap_or_default();
        return kept_policies.contains(&(policy_ns, c.name.clone()));
    }
    false
}

/// Build the repository row from a typed object's pieces. Pure.
#[allow(clippy::too_many_arguments)]
fn repo_row(
    kind: &'static str,
    name: String,
    namespace: Option<String>,
    phase: Option<String>,
    backend: Option<String>,
    mode: String,
    suspended: bool,
    conditions: &[Condition],
    cluster: Option<String>,
    foreign_snapshots: Option<i64>,
    discovered: Option<i64>,
) -> RepoRow {
    let phase = phase.unwrap_or_else(|| EMPTY_CELL.into());
    let maintenance = condition(conditions, MAINTENANCE_CONFIGURED_CONDITION)
        .map(|c| {
            if c.status == "True" {
                "configured".to_string()
            } else {
                c.reason.clone()
            }
        })
        .unwrap_or_else(|| EMPTY_CELL.into());
    let problem = match phase.as_str() {
        "Ready" => None,
        _ => condition(conditions, READY_CONDITION)
            .filter(|c| c.status != "True")
            .map(|c| c.message.clone()),
    };
    RepoRow {
        kind: kind.to_string(),
        name,
        namespace,
        phase,
        backend: backend.unwrap_or_else(|| EMPTY_CELL.into()),
        mode,
        suspended,
        maintenance,
        cluster,
        foreign_snapshots,
        discovered,
        problem,
    }
}

/// Read everything one `status` pass needs from the cluster. Pure IO — no
/// filtering, no interpretation; [`build_report`] does all of that.
pub async fn gather(ctx: &OpsCtx) -> Result<StatusInputs, OpsError> {
    macro_rules! list {
        ($ty:ty, $kind:literal, $plural:literal) => {{
            let api: Api<$ty> = match &ctx.scope {
                Scope::All => Api::all(ctx.client.clone()),
                Scope::Namespace(ns) => Api::namespaced(ctx.client.clone(), ns),
            };
            let ns = match &ctx.scope {
                Scope::All => None,
                Scope::Namespace(ns) => Some(ns.as_str()),
            };
            api.list(&ListParams::default())
                .await
                .map_err(|e| classify_kube("list", $kind, $plural, ns, None, e))?
                .items
        }};
    }

    let repositories = list!(Repository, "Repository", "repositories");

    // ClusterRepositories are cluster-scoped whatever the context's scope is.
    let cluster_repositories = {
        let api: Api<ClusterRepository> = Api::all(ctx.client.clone());
        api.list(&ListParams::default())
            .await
            .map_err(|e| {
                classify_kube(
                    "list",
                    "ClusterRepository",
                    "clusterrepositories",
                    None,
                    None,
                    e,
                )
            })?
            .items
    };

    let policies = list!(SnapshotPolicy, "SnapshotPolicy", "snapshotpolicies");
    let schedules = list!(SnapshotSchedule, "SnapshotSchedule", "snapshotschedules");

    // SnapshotReplications. Listed by hand rather than via `list!` for one
    // reason: the CRD is newer than the rest, so a cluster running an older
    // operator 404s the whole resource — that must render as an empty section,
    // not break the entire overview.
    let snapshot_replications = {
        let api: Api<SnapshotReplication> = match &ctx.scope {
            Scope::All => Api::all(ctx.client.clone()),
            Scope::Namespace(ns) => Api::namespaced(ctx.client.clone(), ns),
        };
        let ns = match &ctx.scope {
            Scope::All => None,
            Scope::Namespace(ns) => Some(ns.as_str()),
        };
        match api.list(&ListParams::default()).await {
            Ok(l) => Some(l.items),
            Err(kube::Error::Api(ae)) if ae.code == 404 => None,
            Err(e) => {
                return Err(classify_kube(
                    "list",
                    "SnapshotReplication",
                    "snapshotreplications",
                    ns,
                    None,
                    e,
                ));
            }
        }
    };

    let snapshots = list!(Snapshot, "Snapshot", "snapshots");
    let restores = list!(Restore, "Restore", "restores");

    Ok(StatusInputs {
        repositories,
        cluster_repositories,
        policies,
        schedules,
        snapshot_replications,
        snapshots,
        restores,
    })
}

/// Join the gathered objects into the report, applying `repo_filter` to every
/// section. Pure — no cluster, no clock.
pub fn build_report(inputs: &StatusInputs, repo_filter: Option<&RepoFilter>) -> StatusReport {
    let mut report = StatusReport::default();

    // Repositories (namespaced) + ClusterRepositories (always cluster-scoped).
    for repo in &inputs.repositories {
        if !repo_matches(
            repo_filter,
            RepositoryKind::Repository,
            &repo.name_any(),
            repo.metadata.namespace.as_deref(),
        ) {
            continue;
        }
        let status = repo.status.as_ref();
        report.repositories.push(repo_row(
            "Repository",
            repo.name_any(),
            repo.metadata.namespace.clone(),
            status
                .and_then(|s| s.phase.as_ref())
                .map(|p| p.label().to_string()),
            status.and_then(|s| s.backend.clone()),
            format!("{:?}", repo.spec.mode),
            repo.spec.suspend,
            status.map(|s| s.conditions.as_slice()).unwrap_or_default(),
            repo.spec
                .identity_defaults
                .as_ref()
                .and_then(|d| d.cluster.clone()),
            status
                .and_then(|s| s.catalog.as_ref())
                .and_then(|c| c.foreign_snapshot_count),
            status
                .and_then(|s| s.catalog.as_ref())
                .and_then(|c| c.discovered_backup_count),
        ));
    }
    for repo in &inputs.cluster_repositories {
        if !repo_matches(
            repo_filter,
            RepositoryKind::ClusterRepository,
            &repo.name_any(),
            None,
        ) {
            continue;
        }
        let status = repo.status.as_ref();
        report.repositories.push(repo_row(
            "ClusterRepository",
            repo.name_any(),
            None,
            status
                .and_then(|s| s.phase.as_ref())
                .map(|p| p.label().to_string()),
            status.and_then(|s| s.backend.clone()),
            format!("{:?}", repo.spec.mode),
            repo.spec.suspend,
            status.map(|s| s.conditions.as_slice()).unwrap_or_default(),
            repo.spec
                .identity_defaults
                .as_ref()
                .and_then(|d| d.cluster.clone()),
            status
                .and_then(|s| s.catalog.as_ref())
                .and_then(|c| c.foreign_snapshot_count),
            status
                .and_then(|s| s.catalog.as_ref())
                .and_then(|c| c.discovered_backup_count),
        ));
    }

    // Policies + their schedules.
    let kept_policies: Vec<&SnapshotPolicy> = inputs
        .policies
        .iter()
        .filter(|p| policy_matches(repo_filter, p))
        .collect();
    // Keyed by (namespace, name): policyRefs are namespace-local, and two
    // namespaces may both have a policy named `nightly`.
    let kept_keys: std::collections::BTreeSet<(String, String)> = kept_policies
        .iter()
        .map(|p| {
            (
                p.metadata.namespace.clone().unwrap_or_default(),
                p.name_any(),
            )
        })
        .collect();
    for policy in &kept_policies {
        let status = policy.status.as_ref();
        report.policies.push(PolicyRow {
            name: policy.name_any(),
            namespace: policy.metadata.namespace.clone().unwrap_or_default(),
            repository: policy_repository_display(&policy.spec),
            suspended: policy.spec.suspend,
            last_snapshot: status.and_then(|s| s.last_successful_snapshot.clone()),
            last_verified: status.and_then(|s| s.last_verified.clone()),
        });
        if let Some(c) = status
            .map(|s| s.conditions.as_slice())
            .unwrap_or_default()
            .iter()
            .find(|c| c.type_ == STALLED_CONDITION && c.status == "True")
        {
            report.stalled.push(StalledRow {
                kind: "SnapshotPolicy".to_string(),
                object: format!(
                    "{}/{}",
                    policy.metadata.namespace.clone().unwrap_or_default(),
                    policy.name_any()
                ),
                message: c.message.clone(),
            });
        }
    }
    for schedule in &inputs.schedules {
        let policy = schedule
            .spec
            .policy_ref
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "(selector)".to_string());
        // Under --repository, keep a schedule only when its policy was kept
        // (selector-based schedules are kept — their fan-out is dynamic).
        if repo_filter.is_some()
            && schedule.spec.policy_ref.is_some()
            && !kept_keys.contains(&(
                schedule.metadata.namespace.clone().unwrap_or_default(),
                policy.clone(),
            ))
        {
            continue;
        }
        let status = schedule.status.as_ref();
        report.schedules.push(ScheduleRow {
            name: schedule.name_any(),
            namespace: schedule.metadata.namespace.clone().unwrap_or_default(),
            policy,
            cron: schedule.spec.schedule.cron.clone(),
            suspended: schedule.spec.schedule.suspend,
            last_fire: status
                .and_then(|s| s.last_schedule.as_ref())
                .and_then(|l| l.at.clone()),
            next_fire: status
                .and_then(|s| s.next_schedule.as_ref())
                .and_then(|n| n.at.clone()),
            consecutive_failures: status
                .and_then(|s| s.consecutive_failures)
                .filter(|&n| n > 0),
        });
    }

    // SnapshotReplications — absent when the CRD is not installed, which
    // renders as an empty section rather than breaking the overview.
    for r in inputs.snapshot_replications.iter().flatten() {
        if !replication_matches(repo_filter, r) {
            continue;
        }
        if let Some(c) = r
            .status
            .as_ref()
            .map(|s| s.conditions.as_slice())
            .unwrap_or_default()
            .iter()
            .find(|c| c.type_ == STALLED_CONDITION && c.status == "True")
        {
            report.stalled.push(StalledRow {
                kind: "SnapshotReplication".to_string(),
                object: format!(
                    "{}/{}",
                    r.metadata.namespace.clone().unwrap_or_default(),
                    r.name_any()
                ),
                message: c.message.clone(),
            });
        }
        report
            .snapshot_replications
            .push(snapshot_replication_row(r));
    }

    // In-flight + stalled work.
    for snap in &inputs.snapshots {
        let status = snap.status.as_ref();
        // UID-label (discovered) / pinned-resolved-ref (produced) matching —
        // the same logic `snapshots list --repository` uses.
        if let Some(f) = repo_filter
            && !matches_repository(snap, f)
        {
            continue;
        }
        if snapshot_in_flight(status.and_then(|s| s.phase.as_ref())) {
            report.in_flight.snapshots += 1;
        }
        if let Some(c) = status
            .map(|s| s.conditions.as_slice())
            .unwrap_or_default()
            .iter()
            .find(|c| c.type_ == STALLED_CONDITION && c.status == "True")
        {
            report.stalled.push(StalledRow {
                kind: "Snapshot".to_string(),
                object: format!(
                    "{}/{}",
                    snap.metadata.namespace.clone().unwrap_or_default(),
                    snap.name_any()
                ),
                message: c.message.clone(),
            });
        }
    }
    for restore in &inputs.restores {
        if !restore_matches(repo_filter, &kept_keys, restore) {
            continue;
        }
        let status = restore.status.as_ref();
        if restore_in_flight(status.and_then(|s| s.phase.as_ref())) {
            report.in_flight.restores += 1;
        }
        if let Some(c) = status
            .map(|s| s.conditions.as_slice())
            .unwrap_or_default()
            .iter()
            .find(|c| c.type_ == STALLED_CONDITION && c.status == "True")
        {
            report.stalled.push(StalledRow {
                kind: "Restore".to_string(),
                object: format!(
                    "{}/{}",
                    restore.metadata.namespace.clone().unwrap_or_default(),
                    restore.name_any()
                ),
                message: c.message.clone(),
            });
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::testutil::from_yaml;

    #[test]
    fn build_report_counts_in_flight_and_stalled() {
        let repo: Repository = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: { name: nas, namespace: default }
spec:
  backend: { filesystem: { path: /repo } }
  encryption: { passwordSecretRef: { name: pw, key: password } }
status: { phase: Ready, backend: filesystem }
"#,
        );
        let snap: Snapshot = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: { name: s1, namespace: default }
spec: { policyRef: { name: nightly } }
status: { phase: Running }
"#,
        );
        let inputs = StatusInputs {
            repositories: vec![repo],
            cluster_repositories: vec![],
            policies: vec![],
            schedules: vec![],
            snapshot_replications: None,
            snapshots: vec![snap],
            restores: vec![],
        };
        let report = build_report(&inputs, None);
        assert_eq!(report.repositories.len(), 1);
        assert_eq!(report.in_flight.snapshots, 1);
        assert!(report.snapshot_replications.is_empty());
    }

    /// The `--repository` filter reaches every section at once: a repository
    /// row, its policy, that policy's schedule, and the snapshots pinned to it
    /// survive; everything belonging to the other repository is dropped.
    #[test]
    fn build_report_applies_the_repository_filter_across_sections() {
        let repos: Vec<Repository> = ["nas", "offsite"]
            .iter()
            .map(|name| {
                from_yaml(&format!(
                    r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Repository
metadata: {{ name: {name}, namespace: media }}
spec:
  backend: {{ filesystem: {{ path: /repo }} }}
  encryption: {{ passwordSecretRef: {{ name: pw, key: password }} }}
status: {{ phase: Ready, backend: filesystem }}
"#
                ))
            })
            .collect();
        let policies: Vec<SnapshotPolicy> = ["nas", "offsite"]
            .iter()
            .map(|repo| {
                from_yaml(&format!(
                    r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: {{ name: p-{repo}, namespace: media }}
spec:
  repository: {{ name: {repo} }}
  sources: [ {{ pvc: {{ name: data }} }} ]
"#
                ))
            })
            .collect();
        let schedules: Vec<SnapshotSchedule> = ["nas", "offsite"]
            .iter()
            .map(|repo| {
                from_yaml(&format!(
                    r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotSchedule
metadata: {{ name: s-{repo}, namespace: media }}
spec:
  policyRef: {{ name: p-{repo} }}
  schedule: {{ cron: "0 3 * * *" }}
"#
                ))
            })
            .collect();
        let snapshots: Vec<Snapshot> = ["nas", "offsite"]
            .iter()
            .map(|repo| {
                from_yaml(&format!(
                    r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: {{ name: snap-{repo}, namespace: media }}
spec: {{ policyRef: {{ name: p-{repo} }} }}
status:
  phase: Running
  resolved: {{ repository: {{ kind: Repository, name: {repo} }} }}
"#
                ))
            })
            .collect();
        let inputs = StatusInputs {
            repositories: repos,
            policies,
            schedules,
            snapshots,
            ..StatusInputs::default()
        };
        let filter = RepoFilter {
            uid: "uid-nas".into(),
            name: "nas".into(),
            kind: RepositoryKind::Repository,
            namespace: Some("media".into()),
        };

        let all = build_report(&inputs, None);
        assert_eq!(all.repositories.len(), 2);
        assert_eq!(all.in_flight.snapshots, 2);

        let filtered = build_report(&inputs, Some(&filter));
        assert_eq!(
            filtered
                .repositories
                .iter()
                .map(|r| &r.name)
                .collect::<Vec<_>>(),
            vec!["nas"]
        );
        assert_eq!(
            filtered
                .policies
                .iter()
                .map(|p| &p.name)
                .collect::<Vec<_>>(),
            vec!["p-nas"]
        );
        assert_eq!(
            filtered
                .schedules
                .iter()
                .map(|s| &s.name)
                .collect::<Vec<_>>(),
            vec!["s-nas"]
        );
        // Only the snapshot pinned to `nas` counts as in flight.
        assert_eq!(filtered.in_flight.snapshots, 1);
    }

    /// `CatalogStatus` is shared by both repository kinds, so one accessor
    /// chain serves `Repository` and `ClusterRepository` alike — this pins the
    /// field path `build_report` reads.
    #[test]
    fn the_discovered_count_comes_off_the_catalog_status_of_either_kind() {
        let repo: Repository = serde_json::from_value(serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "Repository",
            "metadata": { "name": "nas", "namespace": "media" },
            "spec": {
                "backend": { "filesystem": { "path": "/repo" } },
                "encryption": { "passwordSecretRef": { "name": "creds" } }
            },
            "status": { "phase": "Ready", "catalog": { "discoveredBackupCount": 4 } },
        }))
        .expect("repository fixture");
        let cluster_repo: ClusterRepository = serde_json::from_value(serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "ClusterRepository",
            "metadata": { "name": "offsite" },
            "spec": {
                "backend": { "filesystem": { "path": "/repo" } },
                "encryption": { "passwordSecretRef": { "name": "creds", "namespace": "kopiur" } },
                "allowedNamespaces": { "all": true }
            },
            // Scanned, but nothing was there to materialize.
            "status": { "phase": "Ready", "catalog": { "discoveredBackupCount": 0 } },
        }))
        .expect("cluster repository fixture");
        // No catalog at all — never scanned.
        let unscanned: Repository = serde_json::from_value(serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "Repository",
            "metadata": { "name": "fresh", "namespace": "media" },
            "spec": {
                "backend": { "filesystem": { "path": "/repo" } },
                "encryption": { "passwordSecretRef": { "name": "creds" } }
            },
            "status": { "phase": "Ready" },
        }))
        .expect("repository fixture");

        // The accessor chain `build_report` uses, per kind.
        let ns_discovered = repo
            .status
            .as_ref()
            .and_then(|s| s.catalog.as_ref())
            .and_then(|c| c.discovered_backup_count);
        let cluster_discovered = cluster_repo
            .status
            .as_ref()
            .and_then(|s| s.catalog.as_ref())
            .and_then(|c| c.discovered_backup_count);
        let unscanned_discovered = unscanned
            .status
            .as_ref()
            .and_then(|s| s.catalog.as_ref())
            .and_then(|c| c.discovered_backup_count);
        assert_eq!(ns_discovered, Some(4));
        // A scanned-but-empty repository reports `0`, distinct from never-scanned.
        assert_eq!(cluster_discovered, Some(0));
        assert_eq!(unscanned_discovered, None);

        let row = repo_row(
            "Repository",
            "nas".into(),
            Some("media".into()),
            Some("Ready".into()),
            Some("Filesystem".into()),
            "ReadWrite".into(),
            false,
            &[],
            None,
            None,
            ns_discovered,
        );
        assert_eq!(row.discovered, Some(4));
    }

    #[test]
    fn replication_filter_matches_source_or_destination_ref() {
        let filter = RepoFilter {
            uid: "u1".into(),
            name: "nas".into(),
            kind: RepositoryKind::Repository,
            namespace: Some("media".into()),
        };
        let repl = |source: serde_json::Value, dest: serde_json::Value| -> SnapshotReplication {
            serde_json::from_value(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1", "kind": "SnapshotReplication",
                "metadata": { "name": "r", "namespace": "media" },
                "spec": {
                    "sourceRef": source,
                    "destinationRef": dest,
                    "schedule": { "cron": "0 6 * * *" },
                }
            }))
            .unwrap()
        };
        let other = serde_json::json!({ "kind": "ClusterRepository", "name": "offsite" });
        let nas = serde_json::json!({ "kind": "Repository", "name": "nas" });
        // Matches as source (ref ns absent = CR ns) and as destination.
        assert!(replication_matches(
            Some(&filter),
            &repl(nas.clone(), other.clone())
        ));
        assert!(replication_matches(
            Some(&filter),
            &repl(other.clone(), nas.clone())
        ));
        // The same repo name in ANOTHER namespace must not match.
        let elsewhere =
            serde_json::json!({ "kind": "Repository", "name": "nas", "namespace": "other" });
        assert!(!replication_matches(
            Some(&filter),
            &repl(elsewhere, other.clone())
        ));
        // No filter keeps everything.
        assert!(replication_matches(None, &repl(other.clone(), other)));
    }

    #[test]
    fn policy_filter_matches_any_of_the_repository_set() {
        let filter = RepoFilter {
            uid: "u1".into(),
            name: "nas".into(),
            kind: RepositoryKind::Repository,
            namespace: Some("media".into()),
        };
        let policy = |repo_surface: serde_json::Value| -> SnapshotPolicy {
            let mut spec = serde_json::json!({
                "sources": [ { "pvc": { "name": "d" } } ],
            });
            spec.as_object_mut()
                .unwrap()
                .extend(repo_surface.as_object().unwrap().clone());
            serde_json::from_value(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1", "kind": "SnapshotPolicy",
                "metadata": { "name": "p", "namespace": "media" },
                "spec": spec,
            }))
            .unwrap()
        };
        // Single-repo: matches (ref ns absent = policy ns).
        assert!(policy_matches(
            Some(&filter),
            &policy(serde_json::json!({ "repository": { "name": "nas" } }))
        ));
        // Multi-repo: ANY-of over `spec.repositories` — the filtered repo may
        // be any member, so the policy shows up under every repo it targets.
        assert!(policy_matches(
            Some(&filter),
            &policy(serde_json::json!({ "repositories": [
                { "kind": "ClusterRepository", "name": "offsite" },
                { "name": "nas" },
            ] }))
        ));
        // …but not when no member matches (same name, wrong namespace).
        assert!(!policy_matches(
            Some(&filter),
            &policy(serde_json::json!({ "repositories": [
                { "name": "nas", "namespace": "other" },
                { "kind": "ClusterRepository", "name": "offsite" },
            ] }))
        ));
        // No filter keeps everything.
        assert!(policy_matches(
            None,
            &policy(serde_json::json!({ "repositories": [ { "name": "x" } ] }))
        ));
    }

    #[test]
    fn restore_filter_matches_resolved_spec_or_kept_policy() {
        let filter = RepoFilter {
            uid: "u1".into(),
            name: "nas".into(),
            kind: RepositoryKind::Repository,
            namespace: Some("media".into()),
        };
        let kept: std::collections::BTreeSet<(String, String)> =
            [("media".to_string(), "nightly".to_string())].into();
        let restore = |v: serde_json::Value| -> Restore { serde_json::from_value(v).unwrap() };

        // Pinned resolved repository matches (ref ns absent = restore ns).
        let pinned = restore(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1", "kind": "Restore",
            "metadata": { "name": "r", "namespace": "media" },
            "spec": { "source": { "snapshotRef": { "name": "s" } }, "target": { "pvcRef": { "name": "d" } } },
            "status": { "resolved": { "repository": { "kind": "Repository", "name": "nas" } } }
        }));
        assert!(restore_matches(Some(&filter), &kept, &pinned));

        // fromPolicy source matches through the kept (namespace, name) key…
        let from_policy = restore(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1", "kind": "Restore",
            "metadata": { "name": "r", "namespace": "media" },
            "spec": { "source": { "fromPolicy": { "name": "nightly" } }, "target": { "pvcRef": { "name": "d" } } }
        }));
        assert!(restore_matches(Some(&filter), &kept, &from_policy));

        // …but the SAME policy name in another namespace must NOT match.
        let other_ns = restore(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1", "kind": "Restore",
            "metadata": { "name": "r", "namespace": "other" },
            "spec": { "source": { "fromPolicy": { "name": "nightly" } }, "target": { "pvcRef": { "name": "d" } } }
        }));
        assert!(!restore_matches(Some(&filter), &kept, &other_ns));

        // No filter keeps everything.
        assert!(restore_matches(None, &kept, &other_ns));
    }

    #[test]
    fn in_flight_classification_is_exhaustive() {
        assert!(snapshot_in_flight(Some(&SnapshotPhase::Pending)));
        assert!(snapshot_in_flight(Some(&SnapshotPhase::Running)));
        assert!(!snapshot_in_flight(Some(&SnapshotPhase::Succeeded)));
        assert!(!snapshot_in_flight(Some(&SnapshotPhase::Discovered)));
        assert!(restore_in_flight(Some(&RestorePhase::Resolving)));
        assert!(!restore_in_flight(Some(&RestorePhase::Completed)));
        // A phase from a NEWER operator is surfaced as in-flight, never
        // silently dropped from the totals (#359 version-skew class).
        assert!(snapshot_in_flight(Some(&SnapshotPhase::Unknown(
            "Quiescing".into()
        ))));
        assert!(restore_in_flight(Some(&RestorePhase::Unknown(
            "Staging".into()
        ))));
    }
}
