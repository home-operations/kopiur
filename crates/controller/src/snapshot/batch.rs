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
use kopiur_api::common::{NamespaceDeletePolicy, RepositoryKind, RepositoryRef};
use kopiur_mover::workspec::SnapshotAnchor;
use kube::{Resource, ResourceExt};

use super::plan::{
    BreakerState, DeletionFacts, DeletionPlan, OwnerState, effective_deletion_policy,
    effective_on_schedule_delete, pinned_repository, plan_deletion, pruned_by, resolve_origin,
    schedule_owner_ref,
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
    /// Whether the CR carries a `status.resolved.repository` pin (pre-#255 CRs
    /// have none). The COUNTING set treats a pin-less CR as matching EVERY key
    /// (over-count is fail-safe; see [`repo_matches`]); the FIRE set uses this
    /// to enroll a pin-less PEER into NO repository's batch — only its own
    /// self-fire, whose live repo resolution is authoritative, drains it
    /// ([`fire_eligible`], IMPORTANT-4).
    pub pinned: bool,
    /// Whether the CR carries a `SnapshotSchedule` controller ownerRef
    /// ([`schedule_owner_ref`]). The FIRE set drops a schedule-OWNED peer while
    /// the schedule reflector store is unsynced (its owner state defaults to
    /// `Alive`, turning the cascade guard off — a cascade-retain child would
    /// look fireable); it stays in the COUNTING set regardless
    /// ([`fire_eligible`], IMPORTANT-3a).
    pub schedule_owned: bool,
}

/// The `Snapshot`'s `metadata.deletionTimestamp`, converted from the
/// k8s-openapi `Time` (a jiff `Timestamp`) to `chrono`. `None` if unset or
/// unrepresentable.
fn deletion_timestamp_utc(backup: &Snapshot) -> Option<chrono::DateTime<chrono::Utc>> {
    let t = backup.meta().deletion_timestamp.as_ref()?;
    chrono::DateTime::from_timestamp(t.0.as_second(), 0)
}

/// The `SnapshotAnchor` self-heal identity for a `Snapshot`, built from its
/// pinned `status.snapshot`/`status.timing` (the same source fields
/// [`super::build::snapshot_anchor`] reads).
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
        pinned: pinned_repository(backup).is_some(),
        schedule_owned: schedule_owner_ref(backup).is_some(),
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

/// Everything the FIRE-eligibility filter ([`fire_eligible`]) needs, all pure.
///
/// The counting set ([`pending_members`]) is maximally INCLUSIVE — over-count
/// trips the breaker EARLIER, and an extra candidate is still gated downstream,
/// so over-counting is the fail-safe direction there. The FIRE set is the polar
/// opposite: maximally EXCLUSIVE. An excluded member is not deleted this pass; it
/// merely requeues and is retried (draining via its OWN reconcile's self-fire, or
/// once the excluding condition clears), so UNDER-firing is the fail-safe
/// direction here. Every field below narrows the FIRE set past the counting set.
pub struct FireEligibility<'a> {
    /// The triggering CR's uid. It is ALWAYS fireable: the finalizer path
    /// ([`super::handle_deletion`]) already authorized it with LIVE fallbacks
    /// (its owner/breaker/namespace state resolved authoritatively, not from a
    /// possibly-cold store), and the repo the batch targets IS its own live
    /// resolution. The exclusions below therefore apply only to PEERS swept in
    /// by the inclusive counting set.
    pub self_uid: &'a str,
    /// The repository's mass-deletion breaker threshold (`0` disables it).
    pub threshold: u32,
    /// The repository's `allow-mass-deletion` ack, parsed + clock-skew-clamped
    /// exactly as the breaker does ([`super::plan::parse_mass_deletion_ack`]).
    pub ack: Option<chrono::DateTime<chrono::Utc>>,
    /// The repository's namespace-deletion cascade policy — a terminating-
    /// namespace peer is fireable ONLY under `Delete`.
    pub on_namespace_delete: NamespaceDeletePolicy,
    /// The member namespaces observed TERMINATING at fire time (resolved once
    /// per distinct namespace by the caller). A namespace whose terminating-ness
    /// could not be read is treated as terminating here — fail-safe UNDER-fire.
    pub terminating_namespaces: &'a HashSet<String>,
    /// Whether the `SnapshotSchedule` reflector store is synced. While `false`,
    /// a schedule-owned EXTERNAL peer is excluded (its owner state would default
    /// to `Alive`, disabling the cascade guard).
    pub schedule_synced: bool,
}

/// The FIRE set: the subset of the inclusive `pending` (counting) set that this
/// reconcile may actually enroll into a new batch Job. Maximally EXCLUSIVE
/// (fail-safe = under-fire) — the four review findings, one pass:
///
/// - CRITICAL-1: while the breaker is TRIPPING (`threshold > 0 && unacked >=
///   threshold`, counted over the FULL pending set), a HELD external peer
///   (`external && deletion_timestamp > ack`) is excluded — a breaker-exempt
///   trigger (an operator prune, or an acked older wave) must never drain the
///   held wave's kopia data without an ack.
/// - IMPORTANT-2: a peer whose namespace is terminating is excluded unless the
///   repository's `onNamespaceDelete` is `Delete` (an `Orphan`-destined member
///   plans `OrphanSnapshot` on its own reconcile — never delete it here).
/// - IMPORTANT-3a: while the schedule store is unsynced, a schedule-owned
///   EXTERNAL peer is excluded (cascade-retain-destined children would look
///   fireable when the owner lookup defaults to `Alive`).
/// - IMPORTANT-4: a pin-less PEER is excluded — the counting set matches it to
///   EVERY repo key, but its `{snapshot_id, anchor}` must not ride an UNRELATED
///   repo's batch (against a replication TARGET that deletes the replica).
///
/// The triggering `self_uid` bypasses every exclusion (see [`FireEligibility`]).
pub fn fire_eligible(pending: Vec<PendingMember>, cx: &FireEligibility<'_>) -> Vec<PendingMember> {
    // Whether the breaker is tripping is decided over the FULL (inclusive)
    // pending set — before any fire-path narrowing — so a peer's held-ness is
    // judged against the same total the breaker itself would see.
    let unacked = unacked_breaker_count(&pending, cx.ack);
    let breaker_tripping = cx.threshold > 0 && unacked >= cx.threshold as usize;
    pending
        .into_iter()
        .filter(|m| {
            m.uid == cx.self_uid
                || (fire_pin_ok(m)
                    && fire_schedule_ok(m, cx.schedule_synced)
                    && fire_namespace_ok(m, cx.terminating_namespaces, cx.on_namespace_delete)
                    && fire_breaker_ok(m, breaker_tripping, cx.ack))
        })
        .collect()
}

/// IMPORTANT-4: a pin-less PEER enrolls into NO repo's batch (only its own
/// self-fire, whose live resolution is authoritative, drains it).
fn fire_pin_ok(m: &PendingMember) -> bool {
    m.pinned
}

/// IMPORTANT-3a: exclude a schedule-owned EXTERNAL peer while the schedule store
/// is unsynced. A PRUNE bypasses the cascade guard entirely (owner state is
/// irrelevant to it), so it is never withheld here — retention must keep working
/// even during the store's startup window.
fn fire_schedule_ok(m: &PendingMember, schedule_synced: bool) -> bool {
    schedule_synced || !m.external || !m.schedule_owned
}

/// IMPORTANT-2: a terminating-namespace peer is fireable only under
/// `onNamespaceDelete: Delete`. Exhaustive over [`NamespaceDeletePolicy`] so a
/// new policy variant cannot compile until its fire-eligibility is decided.
fn fire_namespace_ok(
    m: &PendingMember,
    terminating: &HashSet<String>,
    policy: NamespaceDeletePolicy,
) -> bool {
    if !terminating.contains(&m.namespace) {
        return true;
    }
    match policy {
        NamespaceDeletePolicy::Delete => true,
        NamespaceDeletePolicy::Orphan => false,
    }
}

/// CRITICAL-1: while the breaker is tripping, a HELD external peer is excluded.
/// A member is NOT held iff it is a prune (`!external`) or acknowledged
/// (`deletion_timestamp <= ack`). Below threshold (or threshold 0) nothing is
/// excluded — sub-threshold externals were always allowed.
fn fire_breaker_ok(
    m: &PendingMember,
    breaker_tripping: bool,
    ack: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    if !breaker_tripping {
        return true;
    }
    !m.external || ack.is_some_and(|a| m.deletion_timestamp <= a)
}

/// Whether an authoritative bounded (`limit=1`) `SnapshotSchedule` LIST proves
/// the reflector store *trivially* synced. The fire-path schedule exclusion
/// (IMPORTANT-3a) is released by [`super::Context::mark_schedule_synced`], flipped
/// from a SnapshotSchedule reconcile — which NEVER runs on a cluster with zero
/// SnapshotSchedules. An empty authoritative LIST proves there is nothing for the
/// store to be behind on, so the flag can be flipped at startup:
///
/// - `Some(0)` (empty cluster) ⇒ flip.
/// - `Some(n > 0)` ⇒ don't (a reconcile will flip it).
/// - `None` (the LIST failed) ⇒ don't — leave it unsynced (fail-safe: never read
///   a cold store as "no schedule owners").
pub fn empty_schedule_list_proves_synced(item_count: Option<usize>) -> bool {
    item_count == Some(0)
}

/// The subset of `pending` eligible for a NEW batch: excludes any UID already
/// `covered` by a non-FAILED batch Job ([`covered_uids`] — LIVE or SUCCEEDED). THE
/// NO-OVERLAP INVARIANT: a member rides at most one non-terminal-failed batch — this
/// prevents double-enrollment (an anchor-heal double-delete hazard for a LIVE job, a
/// wasteful re-delete / oldest-first stall for a SUCCEEDED-but-draining one) and
/// gives the throttle real parallelism (wave 2 takes the NEXT [`MAX_BATCH_MEMBERS`]).
pub fn fireable_members(
    pending: Vec<PendingMember>,
    covered_uids: &HashSet<String>,
) -> Vec<PendingMember> {
    pending
        .into_iter()
        .filter(|m| !covered_uids.contains(&m.uid))
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
    // UID tiebreak on equal `deletionTimestamp`s: two Snapshots deleted in the
    // same wall-clock second must truncate the SAME way on every reconcile, or
    // the deterministic batch name (`batch_job_name`) would flap between waves
    // and defeat the 409 single-flight dedup.
    sorted.sort_by(|a, b| {
        a.deletion_timestamp
            .cmp(&b.deletion_timestamp)
            .then_with(|| a.uid.cmp(&b.uid))
    });
    sorted.truncate(max);
    sorted
}

/// Deterministic name: same member set (sorted UIDs) => same name. Joins the
/// sorted UIDs with `-` (DNS-safe regardless of UID shape) and lets
/// [`crate::naming::capped_name`] cap+hash — its internal hash is 64-bit
/// FNV-1a, avoiding the 32-bit `short_hash` collision risk for a set-identity
/// name. DNS-63 safe by construction (`capped_name`'s own invariant).
///
/// PRECONDITION: `members` is non-empty. The only caller passes a
/// [`BatchFire::Fire`] payload, which [`batch_fire_decision`] only ever produces
/// non-empty — an empty set would collapse to a repo-only name SHARED across
/// waves, breaking single-flight.
pub fn batch_job_name(repo: &RepositoryRef, members: &[PendingMember]) -> String {
    debug_assert!(
        !members.is_empty(),
        "batch_job_name requires a non-empty member set (Fire is non-empty by construction)"
    );
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

// --- Batch Job classification (the dispatcher's pure decision layer, M5a) ---

/// The terminal (or not) state of a batch delete Job, distilled from its
/// Kubernetes `status` by [`super::batch_job_view`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchJobState {
    /// Not yet terminal (pending or running).
    Live,
    /// Completed successfully — every targeted member was deleted.
    Succeeded,
    /// Terminal failure — at least one member's delete failed.
    Failed,
}

/// A batch delete Job reduced to exactly what the pure classifiers need: its
/// name, the member `Snapshot` UIDs it covers (from
/// [`crate::consts::DELETE_MEMBERS_ANNOTATION`]), its terminal state, and when it
/// went terminal (for the failed-Job reap age). Built by [`super::batch_job_view`]
/// from a live `Job`, so every classifier here is unit-tested without a cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchJobView {
    /// The Job's `metadata.name`.
    pub name: String,
    /// The member `Snapshot` UIDs (the annotation's comma-joined set).
    pub members: Vec<String>,
    /// Terminal state.
    pub state: BatchJobState,
    /// When it went terminal — completion time (success) or the `Failed`
    /// condition's transition time. `None` while `Live` (or a malformed status).
    pub terminal_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// What a deleting `Snapshot` should do this reconcile, decided purely from the
/// batch Jobs covering (or not) its UID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberDisposition {
    /// A LIVE batch Job covers this UID — poll it.
    LiveMember,
    /// A SUCCEEDED batch Job covers this UID — its kopia snapshot is deleted;
    /// release the finalizer.
    SucceededMember,
    /// A FAILED batch Job covers this UID — record the failure and back off.
    FailedMember,
    /// No batch Job covers this UID. `live_batch_exists` is `true` when SOME live
    /// batch Job exists for the repository (this CR just isn't a member) — the
    /// dispatcher then polls rather than tight-looping when it cannot yet fire.
    NotAMember {
        /// Whether any live batch Job exists for this repository.
        live_batch_exists: bool,
    },
}

/// Classify a deleting `Snapshot` (by `this_uid`) against the batch Jobs LISTed
/// for its repository. Precedence is fail-safe: SUCCEEDED (delete confirmed) wins
/// over LIVE (retry in flight) wins over FAILED (needs another wave), so the
/// finalizer is released ONLY on a confirmed success, and a live retry is
/// preferred to reporting failure. A UID in no Job is `NotAMember`, carrying
/// whether any live batch exists for the repo.
pub fn member_disposition(this_uid: &str, jobs: &[BatchJobView]) -> MemberDisposition {
    // Fold every covering Job's state via an EXHAUSTIVE match over
    // `BatchJobState` (no `==`/`matches!` that would silently ignore a future
    // variant): a new state cannot compile until its coverage is decided here.
    let (mut succeeded, mut live, mut failed, mut any_live) = (false, false, false, false);
    for j in jobs {
        let covers = j.members.iter().any(|u| u == this_uid);
        match j.state {
            BatchJobState::Succeeded => succeeded |= covers,
            BatchJobState::Live => {
                live |= covers;
                any_live = true;
            }
            BatchJobState::Failed => failed |= covers,
        }
    }
    // Fail-safe precedence: SUCCEEDED (delete confirmed) > LIVE (retry in flight)
    // > FAILED (needs another wave) — so the finalizer releases ONLY on a
    // confirmed success and a live retry is preferred to reporting failure.
    if succeeded {
        MemberDisposition::SucceededMember
    } else if live {
        MemberDisposition::LiveMember
    } else if failed {
        MemberDisposition::FailedMember
    } else {
        MemberDisposition::NotAMember {
            live_batch_exists: any_live,
        }
    }
}

/// The UIDs already covered by a NON-FAILED batch Job — LIVE (delete in flight) or
/// SUCCEEDED (delete done; the member is merely awaiting its own reconcile to release
/// its finalizer and drain) — the no-overlap exclusion set for [`fireable_members`].
///
/// Excluding SUCCEEDED-covered members too (not only LIVE) is load-bearing: such a
/// member is STILL "pending" to [`pending_members`] (its finalizer is present) even
/// though its kopia snapshot is already gone, so without this exclusion a fresh wave
/// fired before it drains would RE-ENROLL it. That re-enrollment is a wasteful
/// idempotent re-delete in the small case, and — once [`MAX_BATCH_MEMBERS`] truncates
/// oldest-first — a throughput STALL in the large case: every new wave keeps
/// re-selecting the oldest already-deleted members and 409s (or re-deletes) them
/// instead of draining the backlog, serializing a mass deletion behind each wave's
/// finalizer release. Mirrors [`member_disposition`]'s precedence (SUCCEEDED and LIVE
/// both win over FAILED): a member is re-fireable only when its sole coverage is a
/// FAILED job (or none), so a genuine retry is never blocked. Derived from the SAME
/// LIST [`member_disposition`] reads, so the two can never disagree.
pub fn covered_uids(jobs: &[BatchJobView]) -> HashSet<String> {
    let mut covered = HashSet::new();
    for j in jobs {
        // EXHAUSTIVE over `BatchJobState` (not `matches!`): a new terminal state
        // must decide here whether it covers its members, never fall through.
        let covers = match j.state {
            BatchJobState::Live | BatchJobState::Succeeded => true,
            BatchJobState::Failed => false,
        };
        if covers {
            covered.extend(j.members.iter().cloned());
        }
    }
    covered
}

/// A terminal batch Job the dispatcher should reap, and which whole-Job outcome
/// metric to record at the (single) reap point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapTarget {
    /// The Job to delete.
    pub name: String,
    /// The metric to bump — the ONLY place a whole-Job outcome is counted.
    pub outcome: crate::metrics::BatchJobOutcome,
    /// The reaped Job's member count. For a FAILED Job this is the single point
    /// where `kopiur_snapshot_delete_batch_members{outcome="failed"}` is emitted
    /// (once, for every member of the reaped failed Job) — a failed member never
    /// drains its own finalizer, so it has no per-member emission site of its own
    /// (a SUCCEEDED Job's members each emit `deleted` as they drain, so this
    /// count is unused for that outcome).
    pub members: usize,
}

/// A failed batch Job is reaped once it has been terminal this long — a bounded
/// back-off before its members re-fire a fresh wave, matching
/// [`deletion_requeue`]`(`[`DeletionRequeue::JobFailed`]`)`.
pub const FAILED_BATCH_REAP_AGE: Duration = Duration::from_secs(60);

/// Select the terminal batch Jobs eligible to be reaped NOW (batch Jobs carry no
/// `ttlSecondsAfterFinished` — reaping is explicit):
///
/// - a SUCCEEDED Job once NONE of its members still hold the cleanup finalizer
///   (`finalizer_holding`, from the snapshot store) — every member has drained,
///   so deleting the Job cannot strand a member that has not yet observed the
///   success (its own reconcile releases its finalizer off the SUCCEEDED Job
///   first, and the store reflects that before this fires);
/// - a FAILED Job once it has been terminal for `failed_min_age`
///   ([`FAILED_BATCH_REAP_AGE`]) — a bounded back-off before its still-held
///   members re-fire (kopia delete is idempotent, so the retry is safe).
///
/// LIVE Jobs are never reaped. A `None`/future-dated `terminal_at` (missing or
/// clock-skewed status) is treated as not-yet-old-enough — fail-safe: the sweep
/// backstop reaps a genuinely-leaked timestamp-less Job later.
pub fn reap_targets(
    jobs: &[BatchJobView],
    finalizer_holding: &HashSet<String>,
    now: chrono::DateTime<chrono::Utc>,
    failed_min_age: Duration,
) -> Vec<ReapTarget> {
    jobs.iter()
        .filter_map(|j| match j.state {
            BatchJobState::Live => None,
            BatchJobState::Succeeded => j
                .members
                .iter()
                .all(|u| !finalizer_holding.contains(u))
                .then(|| ReapTarget {
                    name: j.name.clone(),
                    outcome: crate::metrics::BatchJobOutcome::Succeeded,
                    members: j.members.len(),
                }),
            BatchJobState::Failed => j
                .terminal_at
                .is_some_and(|t| {
                    now.signed_duration_since(t)
                        .to_std()
                        .is_ok_and(|age| age >= failed_min_age)
                })
                .then(|| ReapTarget {
                    name: j.name.clone(),
                    outcome: crate::metrics::BatchJobOutcome::Failed,
                    members: j.members.len(),
                }),
        })
        .collect()
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
        assert!(members[0].pinned, "a repo-pinned CR is `pinned`");
        assert!(
            !members[0].schedule_owned,
            "the fixture has no SnapshotSchedule ownerRef"
        );
    }

    #[test]
    fn pending_members_flags_schedule_owned_from_the_owner_ref() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
        let mut backup = member_fixture(
            "a",
            true,
            true,
            Some(DeletionPolicy::Delete),
            false,
            false,
            true,
            Some(repo_ref()),
        );
        backup.metadata.owner_references = Some(vec![OwnerReference {
            api_version: crate::consts::API_VERSION.to_string(),
            kind: "SnapshotSchedule".to_string(),
            name: "nightly".to_string(),
            uid: "sched-uid".to_string(),
            controller: Some(true),
            ..Default::default()
        }]);
        let members = pending_members(&[Arc::new(backup)], KEY, alive);
        assert_eq!(members.len(), 1);
        assert!(
            members[0].schedule_owned,
            "a SnapshotSchedule controller ownerRef sets `schedule_owned`"
        );
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
        let matched = pending_members(&[Arc::clone(&s)], KEY, alive);
        assert_eq!(matched.len(), 1);
        assert!(
            !matched[0].pinned,
            "an unpinned CR is `pinned: false` — the FIRE set excludes it as a peer"
        );
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
            // Defaults for the pre-existing (non-fire-eligibility) tests: a
            // pinned, non-schedule-owned member matches the repo key and passes
            // the pin/schedule fire-path filters. `fire_eligible` tests below
            // override these per case.
            pinned: true,
            schedule_owned: false,
        }
    }

    #[test]
    fn fireable_members_excludes_covered_uids() {
        let pending = vec![member("a", 1), member("b", 2), member("c", 3)];
        let covered = HashSet::from(["b".to_string()]);
        let fireable = fireable_members(pending, &covered);
        let uids: Vec<&str> = fireable.iter().map(|m| m.uid.as_str()).collect();
        assert_eq!(uids, vec!["a", "c"]);
    }

    // --- fire_eligible (the exclusive FIRE set; count stays inclusive) --------

    /// A `PendingMember` with every fire-eligibility-relevant flag set explicitly.
    #[allow(clippy::too_many_arguments)]
    fn fmember(
        uid: &str,
        ns: &str,
        secs: i64,
        external: bool,
        pinned: bool,
        schedule_owned: bool,
    ) -> PendingMember {
        PendingMember {
            namespace: ns.into(),
            name: uid.into(),
            uid: uid.into(),
            snapshot_id: format!("k-{uid}"),
            anchor: SnapshotAnchor::default(),
            external,
            deletion_timestamp: chrono::DateTime::from_timestamp(secs, 0).unwrap(),
            pinned,
            schedule_owned,
        }
    }

    /// A `FireEligibility` with benign defaults: no breaker (threshold 0), no
    /// ack, `Delete` ns policy, nothing terminating, schedule synced. Each test
    /// perturbs exactly the axis it exercises.
    fn fire_cx<'a>(self_uid: &'a str, terminating: &'a HashSet<String>) -> FireEligibility<'a> {
        FireEligibility {
            self_uid,
            threshold: 0,
            ack: None,
            on_namespace_delete: NamespaceDeletePolicy::Delete,
            terminating_namespaces: terminating,
            schedule_synced: true,
        }
    }

    /// The member UIDs of a FIRE set, sorted (set-identity, order-independent).
    fn sorted_uids(members: Vec<PendingMember>) -> Vec<String> {
        let mut u: Vec<String> = members.into_iter().map(|m| m.uid).collect();
        u.sort();
        u
    }

    #[test]
    fn fire_eligible_excludes_held_externals_when_breaker_tripping() {
        // THE flagship counterexample (CRITICAL-1): a breaker-exempt trigger — an
        // operator PRUNE (`self`) — fires while a wave of external deletions is
        // HELD. The held externals must NOT ride the prune's batch; only the
        // prune members and any acked externals do.
        let none = HashSet::new();
        let cx = FireEligibility {
            threshold: 2,
            ..fire_cx("self-prune", &none)
        };
        let pending = vec![
            fmember("self-prune", "media", 100, false, true, false), // trigger (prune)
            fmember("prune-peer", "media", 100, false, true, false), // another prune
            fmember("held-1", "media", 300, true, true, false),      // held external
            fmember("held-2", "media", 400, true, true, false),      // held external
        ];
        // unacked externals = 2 (held-1, held-2) >= threshold 2 ⇒ tripping.
        assert_eq!(unacked_breaker_count(&pending, None), 2);
        assert_eq!(
            sorted_uids(fire_eligible(pending, &cx)),
            vec!["prune-peer", "self-prune"],
            "held externals must be excluded; prune members included"
        );
    }

    #[test]
    fn fire_eligible_includes_acked_externals_excludes_newer_unacked() {
        // Stale-ack-inertia (CRITICAL-1): an ACKED older wave (its member is the
        // trigger) must not drain a NEWER unacked wave. ack at t=200 releases the
        // <=200 members; the >200 members are still held (>= threshold).
        let none = HashSet::new();
        let ack = chrono::DateTime::from_timestamp(200, 0);
        let cx = FireEligibility {
            threshold: 2,
            ack,
            ..fire_cx("acked-self", &none)
        };
        let pending = vec![
            fmember("acked-self", "media", 100, true, true, false), // acked (<=200), trigger
            fmember("acked-peer", "media", 200, true, true, false), // acked (<=200)
            fmember("newer-1", "media", 300, true, true, false),    // unacked (>200)
            fmember("newer-2", "media", 400, true, true, false),    // unacked (>200)
        ];
        assert_eq!(unacked_breaker_count(&pending, ack), 2); // newer-1, newer-2
        assert_eq!(
            sorted_uids(fire_eligible(pending, &cx)),
            vec!["acked-peer", "acked-self"],
            "acked members fire; newer unacked wave excluded"
        );
    }

    #[test]
    fn fire_eligible_no_breaker_exclusion_below_threshold_or_disabled() {
        let none = HashSet::new();
        let pending = || {
            vec![
                fmember("self", "media", 100, true, true, false),
                fmember("peer", "media", 200, true, true, false),
            ]
        };
        // Below threshold (2 < 5): nothing excluded.
        let below = FireEligibility {
            threshold: 5,
            ..fire_cx("self", &none)
        };
        assert_eq!(fire_eligible(pending(), &below).len(), 2);
        // Threshold 0 (breaker disabled): nothing excluded, even at high count.
        let disabled = FireEligibility {
            threshold: 0,
            ..fire_cx("self", &none)
        };
        assert_eq!(fire_eligible(pending(), &disabled).len(), 2);
    }

    #[test]
    fn fire_eligible_excludes_unpinned_peers_keeps_unpinned_self() {
        // IMPORTANT-4: an unpinned PEER (matches every key in the count) must not
        // ride this repo's batch; the unpinned SELF (its live resolution IS this
        // repo) always does.
        let none = HashSet::new();
        let cx = fire_cx("self", &none);
        let pending = vec![
            fmember("self", "media", 100, true, false, false), // unpinned trigger
            fmember("pinned-peer", "media", 100, true, true, false),
            fmember("unpinned-peer", "media", 100, true, false, false),
        ];
        assert_eq!(
            sorted_uids(fire_eligible(pending, &cx)),
            vec!["pinned-peer", "self"]
        );
    }

    #[test]
    fn fire_eligible_excludes_terminating_orphan_peer_only() {
        // IMPORTANT-2: a peer in a TERMINATING namespace is fireable only under
        // `onNamespaceDelete: Delete`; under `Orphan` it is excluded (it plans
        // OrphanSnapshot on its own reconcile). A non-terminating peer is always in.
        let terminating: HashSet<String> = HashSet::from(["dying".to_string()]);
        let pending = || {
            vec![
                fmember("self", "live", 100, true, true, false),
                fmember("term-peer", "dying", 100, true, true, false),
                fmember("live-peer", "live", 100, true, true, false),
            ]
        };
        // Orphan: the terminating peer is dropped.
        let orphan = FireEligibility {
            on_namespace_delete: NamespaceDeletePolicy::Orphan,
            ..fire_cx("self", &terminating)
        };
        assert_eq!(
            sorted_uids(fire_eligible(pending(), &orphan)),
            vec!["live-peer", "self"]
        );
        // Delete: the terminating peer cascades and IS fireable.
        let delete = FireEligibility {
            on_namespace_delete: NamespaceDeletePolicy::Delete,
            ..fire_cx("self", &terminating)
        };
        assert_eq!(fire_eligible(pending(), &delete).len(), 3);
    }

    #[test]
    fn fire_eligible_excludes_schedule_owned_external_peer_while_unsynced() {
        // IMPORTANT-3a: while the schedule store is unsynced, a schedule-owned
        // EXTERNAL peer is excluded (its owner lookup defaults to Alive). A
        // schedule-owned PRUNE peer is NOT excluded (prunes bypass the cascade
        // guard — retention must keep working during the startup window).
        let none = HashSet::new();
        let unsynced = FireEligibility {
            schedule_synced: false,
            ..fire_cx("self", &none)
        };
        let pending = vec![
            fmember("self", "media", 100, true, true, false),
            fmember("ext-owned", "media", 100, true, true, true), // external + schedule-owned
            fmember("prune-owned", "media", 100, false, true, true), // prune + schedule-owned
            fmember("ext-unowned", "media", 100, true, true, false),
        ];
        assert_eq!(
            sorted_uids(fire_eligible(pending, &unsynced)),
            vec!["ext-unowned", "prune-owned", "self"],
            "only the external schedule-owned peer is withheld while unsynced"
        );
    }

    #[test]
    fn fire_eligible_synced_includes_schedule_owned_peer() {
        // Same shape, but synced: the schedule-owned external peer is now included
        // (its owner state is trustworthy).
        let none = HashSet::new();
        let cx = fire_cx("self", &none); // schedule_synced: true
        let pending = vec![
            fmember("self", "media", 100, true, true, false),
            fmember("ext-owned", "media", 100, true, true, true),
        ];
        assert_eq!(fire_eligible(pending, &cx).len(), 2);
    }

    #[test]
    fn fire_eligible_always_keeps_the_trigger_even_if_it_would_be_excluded() {
        // The trigger is authoritatively authorized by the finalizer path; even a
        // pin-less, schedule-owned, terminating-Orphan, held-external SELF fires.
        let terminating: HashSet<String> = HashSet::from(["dying".to_string()]);
        let ack = None;
        let cx = FireEligibility {
            threshold: 1,
            ack,
            on_namespace_delete: NamespaceDeletePolicy::Orphan,
            schedule_synced: false,
            ..fire_cx("self", &terminating)
        };
        let pending = vec![
            fmember("self", "dying", 500, true, false, true), // every exclusion axis tripped
            fmember("peer", "dying", 500, true, false, true),
        ];
        assert_eq!(
            sorted_uids(fire_eligible(pending, &cx)),
            vec!["self"],
            "the trigger is never excluded; the peer is"
        );
    }

    #[test]
    fn empty_schedule_list_proves_synced_decision() {
        assert!(empty_schedule_list_proves_synced(Some(0)), "empty ⇒ flip");
        assert!(
            !empty_schedule_list_proves_synced(Some(3)),
            "non-empty ⇒ no"
        );
        assert!(
            !empty_schedule_list_proves_synced(None),
            "LIST error ⇒ no (fail-safe)"
        );
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

    // --- oldest_first_truncated UID tiebreak (deterministic wave membership) ---

    #[test]
    fn oldest_first_truncated_breaks_equal_timestamps_by_uid() {
        // Three members deleted in the SAME second; truncate to 2. The tiebreak
        // must select the two lexicographically-smallest UIDs, deterministically,
        // regardless of input order — otherwise the batch NAME flaps between waves.
        let now = 100;
        let a = vec![
            member("uid-c", now),
            member("uid-a", now),
            member("uid-b", now),
        ];
        let b = vec![
            member("uid-b", now),
            member("uid-c", now),
            member("uid-a", now),
        ];
        let ta_v = oldest_first_truncated(&a, 2);
        let tb_v = oldest_first_truncated(&b, 2);
        let ta: Vec<&str> = ta_v.iter().map(|m| m.uid.as_str()).collect();
        let tb: Vec<&str> = tb_v.iter().map(|m| m.uid.as_str()).collect();
        assert_eq!(ta, vec!["uid-a", "uid-b"]);
        assert_eq!(ta, tb, "same set, different order => identical truncation");
    }

    // --- batch_job_name non-empty precondition ---

    #[test]
    #[should_panic(expected = "non-empty member set")]
    fn batch_job_name_debug_asserts_non_empty() {
        let r = repo(RepositoryKind::Repository, Some("backups"), "nas");
        let _ = batch_job_name(&r, &[]);
    }

    // --- member_disposition matrix ---

    use crate::metrics::BatchJobOutcome;

    fn view(
        name: &str,
        members: &[&str],
        state: BatchJobState,
        terminal_secs: Option<i64>,
    ) -> BatchJobView {
        BatchJobView {
            name: name.into(),
            members: members.iter().map(|s| s.to_string()).collect(),
            state,
            terminal_at: terminal_secs.and_then(|s| chrono::DateTime::from_timestamp(s, 0)),
        }
    }

    #[test]
    fn member_of_a_live_job_is_live() {
        let jobs = vec![view("j1", &["a", "b"], BatchJobState::Live, None)];
        assert_eq!(
            member_disposition("a", &jobs),
            MemberDisposition::LiveMember
        );
    }

    #[test]
    fn member_of_a_succeeded_job_is_succeeded() {
        let jobs = vec![view("j1", &["a", "b"], BatchJobState::Succeeded, Some(1))];
        assert_eq!(
            member_disposition("b", &jobs),
            MemberDisposition::SucceededMember
        );
    }

    #[test]
    fn member_of_a_failed_job_is_failed() {
        let jobs = vec![view("j1", &["a"], BatchJobState::Failed, Some(1))];
        assert_eq!(
            member_disposition("a", &jobs),
            MemberDisposition::FailedMember
        );
    }

    #[test]
    fn succeeded_wins_over_live_and_failed_for_the_same_uid() {
        // A UID that appears in an old FAILED job, a retry LIVE job, AND a SUCCEEDED
        // job must classify as SUCCEEDED (delete confirmed — release the finalizer),
        // never re-report failure or wait.
        let jobs = vec![
            view("old-failed", &["a"], BatchJobState::Failed, Some(1)),
            view("retry-live", &["a"], BatchJobState::Live, None),
            view("done", &["a"], BatchJobState::Succeeded, Some(2)),
        ];
        assert_eq!(
            member_disposition("a", &jobs),
            MemberDisposition::SucceededMember
        );
    }

    #[test]
    fn live_wins_over_failed_for_the_same_uid() {
        // In a FAILED (old) + LIVE (retry) overlap, prefer waiting on the retry.
        let jobs = vec![
            view("old-failed", &["a"], BatchJobState::Failed, Some(1)),
            view("retry-live", &["a"], BatchJobState::Live, None),
        ];
        assert_eq!(
            member_disposition("a", &jobs),
            MemberDisposition::LiveMember
        );
    }

    #[test]
    fn non_member_reports_whether_a_live_repo_batch_exists() {
        // A live batch exists for the repo, this CR is not in it.
        let jobs = vec![view("j1", &["x", "y"], BatchJobState::Live, None)];
        assert_eq!(
            member_disposition("me", &jobs),
            MemberDisposition::NotAMember {
                live_batch_exists: true
            }
        );
        // Only terminal jobs for others: no live batch.
        let jobs = vec![view("j1", &["x"], BatchJobState::Succeeded, Some(1))];
        assert_eq!(
            member_disposition("me", &jobs),
            MemberDisposition::NotAMember {
                live_batch_exists: false
            }
        );
        // No jobs at all.
        assert_eq!(
            member_disposition("me", &[]),
            MemberDisposition::NotAMember {
                live_batch_exists: false
            }
        );
    }

    // --- covered_uids (no-overlap exclusion set: LIVE ∪ SUCCEEDED, not FAILED) ---

    #[test]
    fn covered_uids_unions_live_and_succeeded_but_not_failed() {
        let jobs = vec![
            view("live1", &["a", "b"], BatchJobState::Live, None),
            view("live2", &["c"], BatchJobState::Live, None),
            view("done", &["d"], BatchJobState::Succeeded, Some(1)),
            view("failed", &["e"], BatchJobState::Failed, Some(1)),
        ];
        let uids = covered_uids(&jobs);
        // LIVE (a,b,c) AND SUCCEEDED (d) are covered: a member whose kopia snapshot a
        // succeeded batch already deleted must NOT be re-enrolled while it still holds
        // its finalizer (the re-inclusion stall this exclusion set exists to prevent).
        assert_eq!(
            uids,
            HashSet::from(["a".into(), "b".into(), "c".into(), "d".into()])
        );
        // A member whose ONLY coverage is a FAILED batch stays fireable — a genuine
        // retry is never blocked (mirrors member_disposition's Succeeded/Live > Failed).
        assert!(!uids.contains("e"));
    }

    #[test]
    fn covered_uids_re_enrollment_guard_after_a_succeeded_wave() {
        // Regression for the uncapped concurrent-wave re-inclusion (e2e scenario 6):
        // wave-1 (a,b) SUCCEEDED but not yet drained; wave-2 (c,d) pending. The fireable
        // set for wave-2 must be EXACTLY (c,d) — never re-including the drained-pending
        // a,b — so wave-2's batch is disjoint from wave-1's, not (a,b,c,d).
        let jobs = vec![view(
            "wave1",
            &["a", "b"],
            BatchJobState::Succeeded,
            Some(1),
        )];
        let pending = vec![
            member("a", 1),
            member("b", 2),
            member("c", 3),
            member("d", 4),
        ];
        let fireable = fireable_members(pending, &covered_uids(&jobs));
        let uids: Vec<&str> = fireable.iter().map(|m| m.uid.as_str()).collect();
        assert_eq!(uids, vec!["c", "d"]);
    }

    // --- reap_targets ---

    fn holding(uids: &[&str]) -> HashSet<String> {
        uids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn succeeded_job_reaped_only_once_all_members_drained() {
        let now = chrono::DateTime::from_timestamp(1000, 0).unwrap();
        let jobs = vec![view("done", &["a", "b"], BatchJobState::Succeeded, Some(1))];
        // A member still holding the finalizer spares the Job (not yet drained).
        assert!(reap_targets(&jobs, &holding(&["b"]), now, FAILED_BATCH_REAP_AGE).is_empty());
        // All members drained => reap with the Succeeded outcome (single count point).
        let reaped = reap_targets(&jobs, &holding(&[]), now, FAILED_BATCH_REAP_AGE);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].name, "done");
        assert_eq!(reaped[0].outcome, BatchJobOutcome::Succeeded);
        // Member count is carried for the metric point (unused for Succeeded —
        // its members each emit `deleted` on drain — but still populated).
        assert_eq!(reaped[0].members, 2);
    }

    #[test]
    fn failed_job_reaped_only_after_the_back_off() {
        let terminal = 1000;
        let jobs = vec![view(
            "failed",
            &["a"],
            BatchJobState::Failed,
            Some(terminal),
        )];
        // Members still hold the finalizer (delete failed) — irrelevant to a failed
        // reap; only age gates it.
        let holding = holding(&["a"]);
        // 59s old: too young.
        let young = chrono::DateTime::from_timestamp(terminal + 59, 0).unwrap();
        assert!(reap_targets(&jobs, &holding, young, FAILED_BATCH_REAP_AGE).is_empty());
        // 60s old: reap with the Failed outcome.
        let old = chrono::DateTime::from_timestamp(terminal + 60, 0).unwrap();
        let reaped = reap_targets(&jobs, &holding, old, FAILED_BATCH_REAP_AGE);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].outcome, BatchJobOutcome::Failed);
        // The FAILED reap carries the member count — the single point where the
        // per-member `failed` metric is emitted (once, for every reaped member).
        assert_eq!(reaped[0].members, 1);
    }

    #[test]
    fn live_job_is_never_reaped_and_skewed_timestamps_are_safe() {
        let now = chrono::DateTime::from_timestamp(1000, 0).unwrap();
        // A live job is never a reap target, whatever the finalizer set.
        let live = vec![view("live", &["a"], BatchJobState::Live, None)];
        assert!(reap_targets(&live, &holding(&[]), now, FAILED_BATCH_REAP_AGE).is_empty());
        // A failed job with a FUTURE terminal_at (clock skew) is not-yet-old-enough.
        let future = vec![view("failed", &["a"], BatchJobState::Failed, Some(5000))];
        assert!(reap_targets(&future, &holding(&[]), now, FAILED_BATCH_REAP_AGE).is_empty());
        // A failed job with no terminal_at is spared (the sweep backstop gets it).
        let no_ts = vec![view("failed", &["a"], BatchJobState::Failed, None)];
        assert!(reap_targets(&no_ts, &holding(&[]), now, FAILED_BATCH_REAP_AGE).is_empty());
    }
}
