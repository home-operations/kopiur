//! Pure decision + filtering helpers for the mass-deletion BATCH delete path
//! (M4/M5 wire real IO — Store scans, batch Job dispatch, throttling; nothing
//! in this module is called yet, `mod batch;` only registers it).
//!
//! One repository-scoped batch Job deletes MANY kopia manifest ids over a
//! single connect, instead of one Job per `Snapshot` CR
//! ([`kopiur_mover::workspec::SnapshotDeleteBatchOp`], M1). This module picks
//! which pending `Snapshot` CRs belong to a batch, when to fire it, and what
//! to name/where to run it — all pure and clock-injected, so the whole
//! decision surface is unit-tested without a cluster.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use kopiur_api::Snapshot;
use kopiur_api::common::{RepositoryKind, RepositoryRef};
use kopiur_mover::workspec::SnapshotAnchor;
use kube::{Resource, ResourceExt};

use super::plan::{
    BreakerState, DeletionFacts, DeletionPlan, OwnerState, effective_deletion_policy,
    effective_on_schedule_delete, plan_deletion, pruned_by, resolve_origin,
};

/// Stable per-repo key from the `status.resolved.repository` pin:
/// `"repository:{ns}/{name}"` for a namespaced `Repository`,
/// `"clusterrepository:{name}"` for a `ClusterRepository`. Distinguishes two
/// `Repository`s of the same name in different namespaces, and a `Repository`
/// from a `ClusterRepository` of the same name. Intended for PINNED refs
/// (always namespace-populated for `Repository`, per `pinned_repository_ref`);
/// an unpinned ref's empty namespace degrades gracefully instead of panicking.
pub fn repo_key(r: &RepositoryRef) -> String {
    match r.kind {
        RepositoryKind::Repository => format!(
            "repository:{}/{}",
            r.namespace.as_deref().unwrap_or_default(),
            r.name
        ),
        RepositoryKind::ClusterRepository => format!("clusterrepository:{}", r.name),
    }
}

/// Label-value-safe short form of [`repo_key`] for Job labels: Kubernetes
/// label values forbid `:`/`/`, so this hashes the key rather than embedding
/// it verbatim. `<=40-char name prefix>-<hash8>`, always well under the
/// 63-char label-value limit.
pub fn repo_label(r: &RepositoryRef) -> String {
    let short: String = r.name.chars().take(40).collect();
    let hash = crate::naming::short_hash(&repo_key(r));
    format!("{short}-{hash}")
}

/// One member of a pending batch delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMember {
    /// The `Snapshot` CR's namespace.
    pub namespace: String,
    /// The `Snapshot` CR's name.
    pub name: String,
    /// The `Snapshot` CR's uid — the no-overlap invariant's identity key.
    pub uid: String,
    /// The kopia manifest id to delete.
    pub snapshot_id: String,
    /// Stale-id self-heal anchor (mirrors `SnapshotDeleteOp::anchor`).
    pub anchor: SnapshotAnchor,
    /// No valid pruned-by annotation. An operator prune's own plan (with the
    /// breaker allowed) is ALSO `DeleteSnapshot`, so it IS eligible for the
    /// same batch Job as an external deletion (dispatch is identical either
    /// way) — this flag exists so the breaker-count logic downstream
    /// ([`super::plan::counts_toward_breaker`], stricter than this filter)
    /// can tell the two apart without re-deriving pruned-by itself.
    pub external: bool,
    /// The CR's `metadata.deletionTimestamp`, converted to `chrono`.
    pub deletion_timestamp: chrono::DateTime<chrono::Utc>,
}

/// The `Snapshot`'s `metadata.deletionTimestamp`, converted from the
/// k8s-openapi `Time` (a jiff `Timestamp`) to `chrono`. `None` if unset or
/// unrepresentable.
fn deletion_timestamp_utc(backup: &Snapshot) -> Option<chrono::DateTime<chrono::Utc>> {
    let t = backup.meta().deletion_timestamp.as_ref()?;
    chrono::DateTime::from_timestamp(t.0.as_second(), 0)
}

/// The `SnapshotAnchor` self-heal identity for a `Snapshot`, built from its
/// pinned `status.snapshot`/`status.timing` (mirrors
/// [`super::plan::pinned_mover_identity`]'s source fields).
fn anchor_for(backup: &Snapshot) -> SnapshotAnchor {
    let snap = backup.status.as_ref().and_then(|s| s.snapshot.as_ref());
    let timing = backup.status.as_ref().and_then(|s| s.timing.as_ref());
    SnapshotAnchor {
        source_path: snap
            .and_then(|s| s.identity.source_path.clone())
            .unwrap_or_default(),
        start_time: timing.and_then(|t| t.start_time.clone()),
        username: snap.map(|s| s.identity.username.clone()),
        hostname: snap.map(|s| s.identity.hostname.clone()),
    }
}

/// REPO MATCH RULE: a CR whose `status.resolved.repository` pin matches `key`
/// matches; a CR with NO pin (pre-#255) matches EVERY key. Conservative: an
/// unpinned CR is over-counted into every repository's batch consideration
/// rather than silently excluded from all of them — over-count is the
/// fail-safe direction here (a missed member never gets deleted at all,
/// while an extra candidate is still gated by the plan-would-delete check).
fn repo_matches(backup: &Snapshot, key: &str) -> bool {
    match backup
        .status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .and_then(|r| r.repository.as_ref())
    {
        Some(pinned) => repo_key(pinned) == key,
        None => true,
    }
}

/// Filter a Store snapshot list to the pending destructive set for `key`:
/// `deletionTimestamp` set + our finalizer still present + plan-would-be
/// `DeleteSnapshot` (re-run [`plan_deletion`] with `breaker = Allowed`; this
/// excludes skip-annotated, Retain/Orphan, and cascade-retained via owner
/// state — but NOT an operator prune, whose own plan is also `DeleteSnapshot`
/// for policy `Delete`; see [`PendingMember::external`]) +
/// `status.snapshot.kopiaSnapshotID` present + repo match (see
/// [`repo_matches`]). `owner_lookup` supplies the owner state for each
/// candidate (the caller's schedule Store lookup).
pub fn pending_members(
    snapshots: &[Arc<Snapshot>],
    key: &str,
    owner_lookup: impl Fn(&Snapshot) -> OwnerState,
) -> Vec<PendingMember> {
    snapshots
        .iter()
        .filter_map(|s| pending_member_for(s, key, &owner_lookup))
        .collect()
}

fn pending_member_for(
    backup: &Snapshot,
    key: &str,
    owner_lookup: &impl Fn(&Snapshot) -> OwnerState,
) -> Option<PendingMember> {
    let deletion_timestamp = deletion_timestamp_utc(backup)?;
    if !backup
        .finalizers()
        .iter()
        .any(|f| f == crate::consts::SNAPSHOT_CLEANUP_FINALIZER)
    {
        return None;
    }
    if !repo_matches(backup, key) {
        return None;
    }
    let snapshot_id = backup
        .status
        .as_ref()?
        .snapshot
        .as_ref()?
        .kopia_snapshot_id
        .clone();

    let facts = DeletionFacts {
        policy: effective_deletion_policy(backup.spec.deletion_policy, resolve_origin(backup)),
        annotations: backup.annotations(),
        owner: owner_lookup(backup),
        cascade: effective_on_schedule_delete(backup.spec.on_schedule_delete),
        ns_terminating: false,
        ns_policy: None,
        breaker: BreakerState::Allowed,
    };
    if !matches!(plan_deletion(facts), DeletionPlan::DeleteSnapshot) {
        return None;
    }

    Some(PendingMember {
        namespace: backup.namespace()?,
        name: backup.name_any(),
        uid: backup.uid()?,
        snapshot_id,
        anchor: anchor_for(backup),
        external: pruned_by(backup.annotations()).is_none(),
        deletion_timestamp,
    })
}

/// Count the mass-deletion BREAKER-relevant members among `pending`: EXTERNAL
/// (not operator-pruned — [`PendingMember::external`]) AND unacknowledged
/// (`deletion_timestamp` strictly after the clamped `ack`, or `ack` absent).
/// This is exactly the `unacked_pending_for_repo` argument
/// [`super::plan::breaker_state`] expects — every member here already passed
/// `pending_members`' plan-would-`DeleteSnapshot` filter, so an external one is
/// precisely a [`super::plan::counts_toward_breaker`] deletion.
///
/// Operator prunes are excluded (they ride the same batch Job but must never
/// trip the breaker — retention has to keep working during an incident). An
/// acknowledged deletion (`<= ack`) is excluded because the operator has already
/// approved that wave. Fail-safe: an unparseable ack arrives here as `None`
/// (nothing acknowledged), so a bad ack never shrinks this count.
pub fn unacked_breaker_count(
    pending: &[PendingMember],
    ack: Option<chrono::DateTime<chrono::Utc>>,
) -> usize {
    pending
        .iter()
        .filter(|m| m.external && ack.is_none_or(|a| m.deletion_timestamp > a))
        .count()
}

/// The newest `deletion_timestamp` among the EXTERNAL (breaker-relevant) members
/// — the value to surface in the `allow-mass-deletion` ack command, since
/// acknowledging up to it releases every currently-held external deletion for
/// this repository ("I approve what is pending NOW"). `None` when there are no
/// external members (nothing to acknowledge). Operator prunes are excluded: they
/// are never held, so they must not push the surfaced ack value later than the
/// held set requires.
pub fn newest_pending_deletion(pending: &[PendingMember]) -> Option<chrono::DateTime<chrono::Utc>> {
    pending
        .iter()
        .filter(|m| m.external)
        .map(|m| m.deletion_timestamp)
        .max()
}

/// Cap on members in a single batch delete Job (a huge batch has to be
/// broken into waves for throttling and Job-size sanity).
pub const MAX_BATCH_MEMBERS: usize = 200;
/// How long a burst of pending deletions is allowed to accumulate before it
/// fires (bounded latency vs. bounded batching — small bursts still coalesce).
pub const BATCH_QUIET_WINDOW: Duration = Duration::from_secs(10);

/// The subset of `pending` eligible for a NEW batch: excludes any UID already
/// present in a live batch Job's membership (`in_flight`). THE NO-OVERLAP
/// INVARIANT: a member is in at most one in-flight Job — this both prevents
/// double-enrollment (an anchor-heal double-delete hazard) and gives the
/// throttle real parallelism (wave 2 takes the NEXT [`MAX_BATCH_MEMBERS`]).
pub fn fireable_members(
    pending: Vec<PendingMember>,
    in_flight_uids: &HashSet<String>,
) -> Vec<PendingMember> {
    pending
        .into_iter()
        .filter(|m| !in_flight_uids.contains(&m.uid))
        .collect()
}

/// Whether/what to fire for a repository's fireable set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchFire {
    /// Fire now: oldest-first, truncated to the batch size cap.
    Fire(Vec<PendingMember>),
    /// Not yet — the burst may still be growing.
    Accumulate {
        /// Retry after this long (the remaining quiet window for the oldest
        /// fireable member).
        retry_in: Duration,
    },
}

/// Deterministic, clock-injected: fire when the oldest fireable member's
/// `deletionTimestamp` is `>= quiet_window` old, OR `fireable.len() >= max`.
/// `Accumulate`'s `retry_in` is the remaining window for the oldest member.
pub fn batch_fire_decision(
    fireable: &[PendingMember],
    now: chrono::DateTime<chrono::Utc>,
    quiet_window: Duration,
    max: usize,
) -> BatchFire {
    let Some(oldest) = fireable.iter().map(|m| m.deletion_timestamp).min() else {
        return BatchFire::Accumulate {
            retry_in: quiet_window,
        };
    };
    let age = (now - oldest).to_std().unwrap_or(Duration::ZERO);
    if fireable.len() >= max || age >= quiet_window {
        return BatchFire::Fire(oldest_first_truncated(fireable, max));
    }
    BatchFire::Accumulate {
        retry_in: quiet_window.saturating_sub(age),
    }
}

fn oldest_first_truncated(fireable: &[PendingMember], max: usize) -> Vec<PendingMember> {
    let mut sorted: Vec<PendingMember> = fireable.to_vec();
    sorted.sort_by_key(|m| m.deletion_timestamp);
    sorted.truncate(max);
    sorted
}

/// Deterministic name: same member set (sorted UIDs) => same name. Joins the
/// sorted UIDs with `-` (DNS-safe regardless of UID shape) and lets
/// [`crate::naming::capped_name`] cap+hash — its internal hash is 64-bit
/// FNV-1a, avoiding the 32-bit `short_hash` collision risk for a set-identity
/// name. DNS-63 safe by construction (`capped_name`'s own invariant).
pub fn batch_job_name(repo: &RepositoryRef, members: &[PendingMember]) -> String {
    let mut uids: Vec<&str> = members.iter().map(|m| m.uid.as_str()).collect();
    uids.sort_unstable();
    let full = format!("snapdel-{}-{}", repo_label(repo), uids.join("-"));
    crate::naming::capped_name(&full)
}

/// Whether a new batch Job may launch given how many are already live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottleVerdict {
    /// Under the cap: launch the new batch Job.
    Proceed,
    /// At/over the cap: wait for a live Job to finish first.
    Wait,
}

/// `cap` bounds concurrent live batch delete Jobs cluster-wide
/// (`Context::max_concurrent_delete_jobs`). `None` (the default,
/// `KOPIUR_MAX_CONCURRENT_DELETE_JOBS` unset or `0`) means UNCAPPED: batching
/// (one Job per repository per accumulation window, not one per `Snapshot`)
/// is the primary defense against overwhelming the backend, so an
/// operator-wide concurrency cap is only an opt-in backstop — always
/// `Proceed` when it's off, so a slow/failing repository can never
/// head-of-line-block every other repository's deletions behind it.
pub fn throttle_verdict(live_batch_jobs: usize, cap: Option<NonZeroUsize>) -> ThrottleVerdict {
    match cap {
        None => ThrottleVerdict::Proceed,
        Some(cap) if live_batch_jobs >= cap.get() => ThrottleVerdict::Wait,
        Some(_) => ThrottleVerdict::Proceed,
    }
}

/// Requeue mapping for the deletion path (single source of truth): an enum ->
/// `Duration` fn so call sites can't fat-finger durations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionRequeue {
    /// This CR is already a member of a live batch Job — poll it.
    LiveJob,
    /// Throttled behind the concurrent-batch-Job cap.
    Throttled,
    /// The batch Job failed; back off before retrying.
    JobFailed,
    /// Held by the mass-deletion breaker.
    Held,
    /// Still accumulating a burst; retry after the window remainder.
    Accumulating(Duration),
}

/// The concrete backoff for a [`DeletionRequeue`] reason.
pub fn deletion_requeue(r: DeletionRequeue) -> Duration {
    match r {
        DeletionRequeue::LiveJob => Duration::from_secs(15),
        DeletionRequeue::Throttled => Duration::from_secs(30),
        DeletionRequeue::JobFailed => Duration::from_secs(60),
        DeletionRequeue::Held => Duration::from_secs(300),
        DeletionRequeue::Accumulating(d) => d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use kopiur_api::common::RepositoryRef;
    use kopiur_api::snapshot::SnapshotSpec;
    use kopiur_api::{DeletionPolicy, Snapshot};

    fn repo(kind: RepositoryKind, ns: Option<&str>, name: &str) -> RepositoryRef {
        RepositoryRef {
            kind,
            name: name.to_string(),
            namespace: ns.map(str::to_string),
        }
    }

    // --- repo_key / repo_label -----------------------------------------

    #[test]
    fn repo_key_shapes() {
        assert_eq!(
            repo_key(&repo(RepositoryKind::Repository, Some("backups"), "nas")),
            "repository:backups/nas"
        );
        assert_eq!(
            repo_key(&repo(RepositoryKind::ClusterRepository, None, "shared")),
            "clusterrepository:shared"
        );
    }

    #[test]
    fn repo_key_distinguishes_namespace_and_kind() {
        let a = repo(RepositoryKind::Repository, Some("ns-a"), "nas");
        let b = repo(RepositoryKind::Repository, Some("ns-b"), "nas");
        let c = repo(RepositoryKind::ClusterRepository, None, "nas");
        assert_ne!(repo_key(&a), repo_key(&b));
        assert_ne!(repo_key(&a), repo_key(&c));
    }

    #[test]
    fn repo_label_is_short_and_deterministic() {
        let r = repo(RepositoryKind::Repository, Some("backups"), "nas-primary");
        let label = repo_label(&r);
        assert!(label.len() <= 63, "{} chars", label.len());
        assert_eq!(label, repo_label(&r));
        // A different namespace (same name) must still produce a distinct label
        // (the hash covers the full key, not just the name).
        let other_ns = repo(RepositoryKind::Repository, Some("other"), "nas-primary");
        assert_ne!(label, repo_label(&other_ns));
    }

    // --- pending_members -------------------------------------------------

    const KEY: &str = "repository:backups/nas";

    fn repo_ref() -> RepositoryRef {
        repo(RepositoryKind::Repository, Some("backups"), "nas")
    }

    /// A `Snapshot` fixture shaped for the `pending_members` filter matrix.
    #[allow(clippy::too_many_arguments)]
    fn member_fixture(
        name: &str,
        deleting: bool,
        has_finalizer: bool,
        policy: Option<DeletionPolicy>,
        skip: bool,
        pruned: bool,
        has_snapshot_id: bool,
        pinned_repo: Option<RepositoryRef>,
    ) -> Snapshot {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use kopiur_api::consts::{PRUNED_BY_ANNOTATION, SKIP_SNAPSHOT_CLEANUP_ANNOTATION};
        use kopiur_api::snapshot::{PrunedBy, ResolvedSnapshot, SnapshotInfo, SnapshotStatus};

        let mut backup = Snapshot::new(
            name,
            SnapshotSpec {
                policy_ref: None,
                tags: None,
                failure_policy: None,
                deletion_policy: policy,
                on_schedule_delete: None,
                pin: false,
                description: None,
            },
        );
        backup.metadata.namespace = Some("media".into());
        backup.metadata.uid = Some(format!("uid-{name}"));
        if has_finalizer {
            backup.metadata.finalizers =
                Some(vec![crate::consts::SNAPSHOT_CLEANUP_FINALIZER.to_string()]);
        }
        if deleting {
            backup.metadata.deletion_timestamp = Some(Time(
                k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
            ));
        }
        let mut annotations = BTreeMap::new();
        if skip {
            annotations.insert(SKIP_SNAPSHOT_CLEANUP_ANNOTATION.to_string(), "true".into());
        }
        if pruned {
            annotations.insert(
                PRUNED_BY_ANNOTATION.to_string(),
                PrunedBy::Retention.annotation_value().to_string(),
            );
        }
        backup.metadata.annotations = Some(annotations);

        backup.status = Some(SnapshotStatus {
            snapshot: has_snapshot_id.then(|| SnapshotInfo {
                kopia_snapshot_id: format!("k-{name}"),
                identity: kopiur_api::common::ResolvedIdentity {
                    username: "u".into(),
                    hostname: "h".into(),
                    source_path: Some("/data".into()),
                },
            }),
            resolved: Some(ResolvedSnapshot {
                repository: pinned_repo,
                sources: vec![],
                credential_projection: None,
            }),
            ..Default::default()
        });
        backup
    }

    fn alive(_: &Snapshot) -> OwnerState {
        OwnerState::NoScheduleOwner
    }

    #[test]
    fn pending_members_includes_a_plain_external_delete() {
        let s = Arc::new(member_fixture(
            "a",
            true,
            true,
            Some(DeletionPolicy::Delete),
            false,
            false,
            true,
            Some(repo_ref()),
        ));
        let members = pending_members(&[s], KEY, alive);
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "a");
        assert_eq!(members[0].snapshot_id, "k-a");
        assert!(members[0].external);
    }

    #[test]
    fn pending_members_excludes_not_deleting() {
        let s = Arc::new(member_fixture(
            "a",
            false,
            true,
            Some(DeletionPolicy::Delete),
            false,
            false,
            true,
            Some(repo_ref()),
        ));
        assert!(pending_members(&[s], KEY, alive).is_empty());
    }

    #[test]
    fn pending_members_excludes_finalizer_already_gone() {
        let s = Arc::new(member_fixture(
            "a",
            true,
            false,
            Some(DeletionPolicy::Delete),
            false,
            false,
            true,
            Some(repo_ref()),
        ));
        assert!(pending_members(&[s], KEY, alive).is_empty());
    }

    #[test]
    fn pending_members_excludes_a_retain_plan() {
        let s = Arc::new(member_fixture(
            "a",
            true,
            true,
            Some(DeletionPolicy::Retain),
            false,
            false,
            true,
            Some(repo_ref()),
        ));
        assert!(pending_members(&[s], KEY, alive).is_empty());
    }

    #[test]
    fn pending_members_excludes_skip_annotated() {
        let s = Arc::new(member_fixture(
            "a",
            true,
            true,
            Some(DeletionPolicy::Delete),
            true,
            false,
            true,
            Some(repo_ref()),
        ));
        assert!(pending_members(&[s], KEY, alive).is_empty());
    }

    #[test]
    fn pending_members_includes_operator_pruned_but_marks_it_non_external() {
        // An operator prune's plan (breaker=Allowed) is ALSO DeleteSnapshot for
        // policy: Delete (plan_prune never differs from plan_external in its
        // OUTPUT, only in bypassing the breaker/guard) — so it's dispatched in
        // the same batch Job for efficiency. `counts_toward_breaker` (a
        // separate, narrower function) is what excludes it from the breaker's
        // count; `external: false` here lets that downstream logic tell the
        // two apart without re-deriving pruned-by itself.
        let s = Arc::new(member_fixture(
            "a",
            true,
            true,
            Some(DeletionPolicy::Delete),
            false,
            true,
            true,
            Some(repo_ref()),
        ));
        let members = pending_members(&[s], KEY, alive);
        assert_eq!(members.len(), 1);
        assert!(!members[0].external);
    }

    #[test]
    fn pending_members_excludes_missing_snapshot_id() {
        let s = Arc::new(member_fixture(
            "a",
            true,
            true,
            Some(DeletionPolicy::Delete),
            false,
            false,
            false,
            Some(repo_ref()),
        ));
        assert!(pending_members(&[s], KEY, alive).is_empty());
    }

    #[test]
    fn pending_members_excludes_a_different_repos_pin() {
        let other = repo(RepositoryKind::Repository, Some("other-ns"), "nas");
        let s = Arc::new(member_fixture(
            "a",
            true,
            true,
            Some(DeletionPolicy::Delete),
            false,
            false,
            true,
            Some(other),
        ));
        assert!(pending_members(&[s], KEY, alive).is_empty());
    }

    #[test]
    fn pending_members_unpinned_matches_every_key() {
        // Pre-#255: no pin at all. Conservative over-count — matches every key.
        let s = Arc::new(member_fixture(
            "a",
            true,
            true,
            Some(DeletionPolicy::Delete),
            false,
            false,
            true,
            None,
        ));
        assert_eq!(pending_members(&[Arc::clone(&s)], KEY, alive).len(), 1);
        assert_eq!(
            pending_members(&[s], "clusterrepository:other", alive).len(),
            1
        );
    }

    // --- fireable_members --------------------------------------------------

    fn member(uid: &str, secs: i64) -> PendingMember {
        PendingMember {
            namespace: "media".into(),
            name: uid.into(),
            uid: uid.into(),
            snapshot_id: format!("k-{uid}"),
            anchor: SnapshotAnchor::default(),
            external: true,
            deletion_timestamp: chrono::DateTime::from_timestamp(secs, 0).unwrap(),
        }
    }

    #[test]
    fn fireable_members_excludes_in_flight_uids() {
        let pending = vec![member("a", 1), member("b", 2), member("c", 3)];
        let in_flight = HashSet::from(["b".to_string()]);
        let fireable = fireable_members(pending, &in_flight);
        let uids: Vec<&str> = fireable.iter().map(|m| m.uid.as_str()).collect();
        assert_eq!(uids, vec!["a", "c"]);
    }

    // --- batch_fire_decision -------------------------------------------------

    #[test]
    fn batch_fire_decision_accumulates_when_empty() {
        let now = chrono::DateTime::from_timestamp(100, 0).unwrap();
        match batch_fire_decision(&[], now, Duration::from_secs(10), MAX_BATCH_MEMBERS) {
            BatchFire::Accumulate { retry_in } => assert_eq!(retry_in, Duration::from_secs(10)),
            other => panic!("expected Accumulate, got {other:?}"),
        }
    }

    #[test]
    fn batch_fire_decision_fires_at_the_window_boundary() {
        let quiet = Duration::from_secs(10);
        let oldest = chrono::DateTime::from_timestamp(100, 0).unwrap();
        let members = vec![member("a", 100)];

        // Just under the window: accumulate with the exact remainder.
        let just_under = oldest + chrono::Duration::seconds(9);
        match batch_fire_decision(&members, just_under, quiet, MAX_BATCH_MEMBERS) {
            BatchFire::Accumulate { retry_in } => assert_eq!(retry_in, Duration::from_secs(1)),
            other => panic!("expected Accumulate, got {other:?}"),
        }

        // Exactly at the window: fires (`>=`).
        let at_window = oldest + chrono::Duration::seconds(10);
        match batch_fire_decision(&members, at_window, quiet, MAX_BATCH_MEMBERS) {
            BatchFire::Fire(fired) => assert_eq!(fired.len(), 1),
            other => panic!("expected Fire, got {other:?}"),
        }
    }

    #[test]
    fn batch_fire_decision_max_override_fires_before_the_window() {
        let now = chrono::DateTime::from_timestamp(100, 0).unwrap();
        let members = vec![member("a", 100), member("b", 100), member("c", 100)];
        match batch_fire_decision(&members, now, Duration::from_secs(3600), 2) {
            BatchFire::Fire(fired) => assert_eq!(fired.len(), 2),
            other => panic!("expected Fire, got {other:?}"),
        }
    }

    #[test]
    fn batch_fire_decision_fires_oldest_first_truncated() {
        let now = chrono::DateTime::from_timestamp(1000, 0).unwrap();
        let members = vec![
            member("newest", 900),
            member("oldest", 100),
            member("mid", 500),
        ];
        match batch_fire_decision(&members, now, Duration::from_secs(3600), 2) {
            BatchFire::Fire(fired) => {
                let names: Vec<&str> = fired.iter().map(|m| m.name.as_str()).collect();
                assert_eq!(names, vec!["oldest", "mid"]);
            }
            other => panic!("expected Fire, got {other:?}"),
        }
    }

    // --- batch_job_name -------------------------------------------------

    #[test]
    fn batch_job_name_is_deterministic_and_dns_safe() {
        let r = repo(RepositoryKind::Repository, Some("backups"), "nas");
        let members = vec![member("uid-b", 1), member("uid-a", 2)];
        let name = batch_job_name(&r, &members);
        assert!(name.len() <= 63, "{} chars", name.len());
        // Order-independent: same set, different input order => same name.
        let reordered = vec![member("uid-a", 2), member("uid-b", 1)];
        assert_eq!(name, batch_job_name(&r, &reordered));
    }

    #[test]
    fn batch_job_name_distinct_member_sets_get_distinct_names() {
        let r = repo(RepositoryKind::Repository, Some("backups"), "nas");
        let a = vec![member("uid-a", 1), member("uid-b", 2)];
        let b = vec![member("uid-a", 1), member("uid-c", 2)];
        assert_ne!(batch_job_name(&r, &a), batch_job_name(&r, &b));
    }

    // --- throttle_verdict -------------------------------------------------

    #[test]
    fn throttle_verdict_below_and_at_cap() {
        let cap = NonZeroUsize::new(3);
        assert_eq!(throttle_verdict(2, cap), ThrottleVerdict::Proceed);
        assert_eq!(throttle_verdict(3, cap), ThrottleVerdict::Wait);
    }

    #[test]
    fn throttle_verdict_uncapped_always_proceeds() {
        // `None` (the default) means uncapped: no live count, however large,
        // ever throttles — batching itself is the primary protection.
        assert_eq!(throttle_verdict(0, None), ThrottleVerdict::Proceed);
        assert_eq!(throttle_verdict(1_000_000, None), ThrottleVerdict::Proceed);
    }

    // --- deletion_requeue -------------------------------------------------

    // --- unacked_breaker_count / newest_pending_deletion -----------------

    /// A pending member with an explicit external flag and deletion time.
    fn member_ext(uid: &str, secs: i64, external: bool) -> PendingMember {
        PendingMember {
            external,
            ..member(uid, secs)
        }
    }

    #[test]
    fn unacked_breaker_count_excludes_operator_prunes() {
        // Two external + one operator-pruned (external: false), no ack.
        let pending = vec![
            member_ext("a", 1, true),
            member_ext("b", 2, true),
            member_ext("c", 3, false),
        ];
        assert_eq!(unacked_breaker_count(&pending, None), 2);
    }

    #[test]
    fn unacked_breaker_count_excludes_acknowledged() {
        // deletionTimestamps at 100/200/300; ack at 200 releases 100 and 200
        // (`<= ack`), leaving only the 300 one unacked.
        let pending = vec![
            member_ext("a", 100, true),
            member_ext("b", 200, true),
            member_ext("c", 300, true),
        ];
        let ack = chrono::DateTime::from_timestamp(200, 0);
        assert_eq!(unacked_breaker_count(&pending, ack), 1);
    }

    #[test]
    fn unacked_breaker_count_absent_ack_counts_all_external() {
        let pending = vec![member_ext("a", 100, true), member_ext("b", 200, true)];
        assert_eq!(unacked_breaker_count(&pending, None), 2);
    }

    #[test]
    fn newest_pending_deletion_is_the_max_external_timestamp() {
        // The operator-pruned (later) member must NOT win — only external ones count.
        let pending = vec![
            member_ext("a", 100, true),
            member_ext("b", 300, true),
            member_ext("c", 999, false),
        ];
        assert_eq!(
            newest_pending_deletion(&pending),
            chrono::DateTime::from_timestamp(300, 0)
        );
    }

    #[test]
    fn newest_pending_deletion_none_without_external_members() {
        let pending = vec![member_ext("c", 100, false)];
        assert_eq!(newest_pending_deletion(&pending), None);
        assert_eq!(newest_pending_deletion(&[]), None);
    }

    #[test]
    fn deletion_requeue_mapping() {
        assert_eq!(
            deletion_requeue(DeletionRequeue::LiveJob),
            Duration::from_secs(15)
        );
        assert_eq!(
            deletion_requeue(DeletionRequeue::Throttled),
            Duration::from_secs(30)
        );
        assert_eq!(
            deletion_requeue(DeletionRequeue::JobFailed),
            Duration::from_secs(60)
        );
        assert_eq!(
            deletion_requeue(DeletionRequeue::Held),
            Duration::from_secs(300)
        );
        assert_eq!(
            deletion_requeue(DeletionRequeue::Accumulating(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
    }
}
