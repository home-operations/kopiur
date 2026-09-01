//! Pure decision functions for the `Snapshot` reconciler.
//!
//! Everything here is a pure function over CR/spec/status values — no `ctx`, no
//! kube IO, no `async`. These are the exhaustively-unit-tested decisions the
//! reconcile core in [`super`] wires to the cluster.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kopiur_api::common::{
    CredentialProjection, NamespaceDeletePolicy, RepositoryKind, RepositoryRef,
    ScheduleDeletePolicy,
};
use kopiur_api::consts::PRUNED_BY_ANNOTATION;
use kopiur_api::snapshot::{PrunedBy, SnapshotPhase};
use kopiur_api::{DeletionPolicy, Origin, Snapshot, SnapshotPolicy, SnapshotSchedule};
use kopiur_mover::workspec::MoverWorkSpec;
use kube::{Resource, ResourceExt};

use crate::consts::{API_VERSION, SKIP_SNAPSHOT_CLEANUP_ANNOTATION};
use crate::io;

/// The decision the deletion handler must execute. Derived purely from
/// [`DeletionFacts`] — no IO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionPlan {
    /// Run `kopia snapshot delete <id>` (via a short Job) then remove the
    /// finalizer. On failure, stay in `phase: Deleting` and back off — the CR
    /// is NOT dropped (ADR §4.5).
    DeleteSnapshot,
    /// Remove the finalizer without contacting the repository (snapshot stays).
    /// Used by `Retain`.
    RetainSnapshot,
    /// Remove the finalizer without contacting the repository, record the
    /// snapshot orphaned, emit `SnapshotOrphaned`, bump the orphan metric. Used
    /// by `Orphan` and by the `skip-snapshot-cleanup` annotation escape hatch.
    OrphanSnapshot,
    /// Cascade guard fired: external deletion, owner gone/replaced, stamped
    /// policy Retain, effective policy was Delete. Executor = `RetainSnapshot`'s
    /// (release finalizer, no repo contact) PLUS a Warning event
    /// `SnapshotRetainedOnScheduleDelete` + cascade-retained counter — loud
    /// but not an orphan-metric storm. (Executor lands in M4.)
    RetainSnapshotOnScheduleDelete,
    /// A `SnapshotPolicy`-deletion cascade prune (`pruned-by: policy-cascade`,
    /// stamped by the `SnapshotPolicy` finalizer under `onPolicyDelete: Retain`
    /// — M3) whose Snapshot's own effective `deletionPolicy` is `Delete`. The
    /// whole point of the `policy-cascade` stamp is to NEVER contact the
    /// repository, so this is the loud downgrade: same executor shape as
    /// `RetainSnapshot` (release finalizer, no repo contact) PLUS a Warning
    /// event `SnapshotRetainedOnPolicyDelete` + a policy-cascade-retained
    /// counter — loud but not an orphan-metric storm.
    RetainSnapshotOnPolicyDelete,
    /// Mass-deletion breaker: do NO delete work, keep the finalizer, phase
    /// Deleting + `DeletionHeld=True` condition, requeue long. Drained by the
    /// repo ack annotation. (Executor lands in M4.)
    HoldSnapshotDeletion,
}

/// Observed state of the `Snapshot`'s owning `SnapshotSchedule` at finalizer
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerState {
    /// The ownerRef's schedule exists with the SAME uid and is not terminating.
    Alive,
    /// Gone (404), PRESENT BUT TERMINATING (`deletionTimestamp` set — the
    /// `--cascade=foreground` case), or a same-name schedule with a DIFFERENT
    /// uid (deleted-and-recreated; GC still reaps the old children).
    GoneOrReplaced,
    /// No `SnapshotSchedule` controller ownerRef at all (manual, discovered, or
    /// deliberately orphaned via `kubectl delete --cascade=orphan`): the CR's
    /// own `deletionPolicy` is honored, the cascade guard never fires.
    NoScheduleOwner,
}

/// Per-repository mass-deletion breaker verdict for THIS deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Under the repository's threshold (or acknowledged, or the breaker is
    /// disabled): this deletion may proceed.
    Allowed,
    /// At/over the repository's unacked-pending threshold: hold this deletion.
    Held,
}

/// Everything the deletion decision needs. One struct so the decision table is
/// legible at call sites and in tests.
pub struct DeletionFacts<'a> {
    /// Effective (origin-aware) policy — [`effective_deletion_policy`] output,
    /// so `origin: discovered` is already folded to `Retain` here.
    pub policy: DeletionPolicy,
    /// Skip-cleanup + pruned-by are parsed from here.
    pub annotations: &'a BTreeMap<String, String>,
    /// Observed state of the owning `SnapshotSchedule`, if any.
    pub owner: OwnerState,
    /// [`effective_on_schedule_delete`]`(spec.on_schedule_delete)` — absent →
    /// `Retain`.
    pub cascade: ScheduleDeletePolicy,
    /// Whether the `Snapshot`'s own namespace is terminating.
    pub ns_terminating: bool,
    /// The repository's `onNamespaceDelete`; `None` = repo unresolvable while
    /// the namespace terminates (existing forced-orphan fail-safe).
    pub ns_policy: Option<NamespaceDeletePolicy>,
    /// This repository's mass-deletion breaker verdict for this deletion.
    pub breaker: BreakerState,
}

/// Which operator lifecycle pruned this `Snapshot`, parsed from the
/// `pruned-by` annotation. `None` for anything absent or unrecognized — the
/// finalizer must treat that as an EXTERNAL deletion (fail-safe), never guess
/// "operator".
pub fn pruned_by(annotations: &BTreeMap<String, String>) -> Option<PrunedBy> {
    annotations
        .get(PRUNED_BY_ANNOTATION)
        .and_then(|v| PrunedBy::parse(v))
}

/// Decide what to do on deletion. **Exhaustive** over every enum it touches
/// (`DeletionPolicy`, `ScheduleDeletePolicy`, `NamespaceDeletePolicy`,
/// `OwnerState`, `BreakerState`) with no catch-all: a new variant fails to
/// compile until handled here (ADR §5.5).
///
/// Decision order:
/// 1. `SKIP_SNAPSHOT_CLEANUP_ANNOTATION` present → [`OrphanSnapshot`](DeletionPlan::OrphanSnapshot).
///    Absolute — even over `Held` (it deletes nothing and is the documented
///    per-CR drain lever; ADR §4.5).
/// 2. `ns_terminating`:
///    - `ns_policy == None` → `OrphanSnapshot` (existing fail-safe, unchanged).
///    - `Some(Orphan)` → `OrphanSnapshot` (existing default, unchanged).
///    - `Some(Delete)` → [`plan_ns_delete`]: an EXPLICIT repo-level opt-in.
///      Retention/failed-history prunes keep their operator-prune semantics
///      (step 4), but BOTH the schedule cascade guard (step 3 — during ns
///      teardown the schedule is always gone) AND the IMPLICIT `policy-cascade`
///      Retain stamp are overridden: an unstamped OR `policy-cascade`-stamped
///      child resolves as external destructive (step 5, breaker-gated). The
///      policy cleanup finalizer stamps its live children `policy-cascade`
///      during the SAME teardown (default `onPolicyDelete: Retain`), and letting
///      that quiet-retain downgrade win would silently nullify the opt-in and
///      lose off-site data the user asked to reclaim.
/// 3. Cascade guard (only when not ns-terminating): `pruned_by == None && owner
///    == GoneOrReplaced`:
///    - cascade `Retain` && policy `Delete` → `RetainSnapshotOnScheduleDelete`
///    - cascade `Retain` && policy `Retain` → `RetainSnapshot`
///    - cascade `Retain` && policy `Orphan` → `OrphanSnapshot`
///    - cascade `Delete` → fall through (opt-in cascade; still external ⇒
///      breaker applies).
/// 4. Operator prune (`pruned_by == Some(_)`): match `(prune kind, policy)`
///    exhaustively via [`plan_prune`] — see its doc for the full 3×3 table.
///    NEVER held (retention must keep working during an incident; its rate is
///    bounded elsewhere).
/// 5. External destructive (policy Delete): breaker Held →
///    `HoldSnapshotDeletion`; Allowed → `DeleteSnapshot`.
/// 6. External Retain/Orphan → RetainSnapshot/OrphanSnapshot (never held — no
///    repo contact; holding would wedge CR removal for zero protection).
pub fn plan_deletion(f: DeletionFacts<'_>) -> DeletionPlan {
    if f.annotations.contains_key(SKIP_SNAPSHOT_CLEANUP_ANNOTATION) {
        return DeletionPlan::OrphanSnapshot;
    }
    if f.ns_terminating {
        return plan_ns_terminating(&f);
    }
    plan_live_namespace(&f)
}

/// Step 2: namespace-deletion cascade (ADR-0005 §5). `Delete` is an explicit
/// repo-level opt-in that bypasses the schedule cascade guard (step 3) AND
/// overrides the implicit `policy-cascade` Retain stamp (see [`plan_ns_delete`]),
/// but does not bypass the breaker for external destructive deletes.
fn plan_ns_terminating(f: &DeletionFacts<'_>) -> DeletionPlan {
    match f.ns_policy {
        None => DeletionPlan::OrphanSnapshot,
        Some(NamespaceDeletePolicy::Orphan) => DeletionPlan::OrphanSnapshot,
        Some(NamespaceDeletePolicy::Delete) => plan_ns_delete(f),
    }
}

/// Step 2, the `Some(Delete)` arm: an EXPLICIT `onNamespaceDelete: Delete`
/// repo-level opt-in. **Exhaustive over [`PrunedBy`]** (no catch-all): a new
/// prune kind must decide its namespace-teardown fate here.
///
/// - `None` (unstamped) → external destructive ([`plan_external`], breaker-gated).
/// - `Some(PolicyCascade)` → ALSO external destructive. The opt-in is explicit;
///   the `policy-cascade` stamp is IMPLICIT — the `SnapshotPolicy` cleanup
///   finalizer stamps its live children during this SAME namespace teardown,
///   defaulting to `onPolicyDelete: Retain`. Routing that through [`plan_prune`]
///   would hit the quiet-retain downgrade
///   ([`RetainSnapshotOnPolicyDelete`](DeletionPlan::RetainSnapshotOnPolicyDelete))
///   and silently nullify the opt-in, losing off-site data the user asked to
///   reclaim. So an explicit ns-delete opt-in WINS over the implicit stamp.
///   (The retain-wins-ties rule is only for schedule-vs-policy cascade races,
///   NOT for an explicit namespace-delete opt-in.)
/// - `Some(Retention | FailedHistory | ReplicationRetention | ReplacedRun)` → a
///   genuine operator prune keeps its prune semantics ([`plan_prune`]): never
///   held; effective policy decides.
fn plan_ns_delete(f: &DeletionFacts<'_>) -> DeletionPlan {
    match pruned_by(f.annotations) {
        None | Some(PrunedBy::PolicyCascade) => plan_external(f.policy, f.breaker),
        Some(
            p @ (PrunedBy::Retention
            | PrunedBy::FailedHistory
            | PrunedBy::ReplicationRetention
            | PrunedBy::ReplacedRun),
        ) => plan_prune(p, f.policy),
    }
}

/// Steps 3-6: not namespace-terminating. The cascade guard (step 3) only
/// applies when the owner is gone/replaced; an operator prune bypasses it
/// regardless of owner state (that's step 4, handled inside
/// [`plan_prune_or_external`]).
fn plan_live_namespace(f: &DeletionFacts<'_>) -> DeletionPlan {
    if !cascade_guard_applies(f.owner) || pruned_by(f.annotations).is_some() {
        return plan_prune_or_external(f);
    }
    match plan_cascade_guard(f.cascade, f.policy) {
        Some(plan) => plan,
        None => plan_prune_or_external(f),
    }
}

/// Exhaustive over [`OwnerState`]: only a gone/replaced owner puts the cascade
/// guard in play at all.
fn cascade_guard_applies(owner: OwnerState) -> bool {
    match owner {
        OwnerState::GoneOrReplaced => true,
        OwnerState::Alive | OwnerState::NoScheduleOwner => false,
    }
}

/// Step 3 body. `None` means the opt-in cascade (`cascade == Delete`): the
/// caller falls through to steps 4-6 (still external ⇒ breaker applies).
fn plan_cascade_guard(
    cascade: ScheduleDeletePolicy,
    policy: DeletionPolicy,
) -> Option<DeletionPlan> {
    match cascade {
        ScheduleDeletePolicy::Retain => Some(match policy {
            DeletionPolicy::Delete => DeletionPlan::RetainSnapshotOnScheduleDelete,
            DeletionPolicy::Retain => DeletionPlan::RetainSnapshot,
            DeletionPolicy::Orphan => DeletionPlan::OrphanSnapshot,
        }),
        ScheduleDeletePolicy::Delete => None,
    }
}

/// Steps 4-6: an operator prune (step 4) bypasses the breaker entirely;
/// everything else is external and steps 5-6 apply (the breaker only ever
/// gates `Delete`).
fn plan_prune_or_external(f: &DeletionFacts<'_>) -> DeletionPlan {
    match pruned_by(f.annotations) {
        Some(p) => plan_prune(p, f.policy),
        None => plan_external(f.policy, f.breaker),
    }
}

/// Step 4: operator prune. NEVER held — retention/history-limit pruning must
/// keep working during an incident; its own rate is bounded elsewhere.
///
/// **Exhaustive over both [`PrunedBy`] and [`DeletionPolicy`]** (a flat 5×3
/// match, no catch-all): a new variant of either enum fails to compile until
/// every cell is decided (ADR §5.5).
///
/// | [`PrunedBy`] \\ [`DeletionPolicy`] | `Delete` | `Retain` | `Orphan` |
/// |---|---|---|---|
/// | `Retention` | `DeleteSnapshot` | `RetainSnapshot` | `OrphanSnapshot` |
/// | `FailedHistory` | `DeleteSnapshot` | `RetainSnapshot` | `OrphanSnapshot` |
/// | `PolicyCascade` | [`RetainSnapshotOnPolicyDelete`](DeletionPlan::RetainSnapshotOnPolicyDelete) | `RetainSnapshot` | `OrphanSnapshot` |
/// | `ReplicationRetention` | `DeleteSnapshot` | `RetainSnapshot` | `OrphanSnapshot` |
/// | `ReplacedRun` | `RetainSnapshot` | `RetainSnapshot` | `OrphanSnapshot` |
///
/// The `PolicyCascade`/`Delete` cell is the one loud downgrade: a policy
/// cascade prune under `onPolicyDelete: Retain` never contacts the
/// repository, even though the Snapshot's own effective policy asked for
/// `Delete` — that is the entire reason the finalizer stamps `policy-cascade`
/// instead of leaving the annotation absent.
///
/// `ReplacedRun`/`Delete` is the second, quieter downgrade, and it is a
/// **data-safety** cell rather than a policy one. `concurrencyPolicy: Replace`
/// only ever selects UNFINISHED children, so the victim normally owns no kopia
/// snapshot at all and this executor just releases the finalizer either way.
/// The cell matters solely in the (sub-millisecond, after the executor's live
/// phase re-check) window where a `Running` victim commits its manifest between
/// selection and the delete landing: that CR now owns a real, complete backup,
/// and the user asked to cancel an *in-flight* run — not to destroy a finished
/// one. `RetainSnapshot` leaks instead of losing, which is the only defensible
/// direction for backup software.
///
/// Be precise about what "leaks" means here, because the reclamation is NOT
/// automatic: the kopia snapshot survives with no `Snapshot` CR referencing it,
/// and kopiur does not track it again until the repository's catalog is
/// re-scanned. `catalog.periodicRefresh` is **off by default**
/// (`kopiur_api::common::CatalogBounds`), so nothing re-scans on a timer — the
/// scan happens on a re-bootstrap (a repository spec change), a
/// failure re-probe, or an explicit `catalog-scan-requested-at` request. Only
/// after that scan does the snapshot become a `Discovered` row that adoption and
/// GFS retention can govern. Until then it is untracked repository data, which
/// is the correct trade for never destroying a completed backup.
fn plan_prune(pruned: PrunedBy, policy: DeletionPolicy) -> DeletionPlan {
    match (pruned, policy) {
        (PrunedBy::Retention, DeletionPolicy::Delete) => DeletionPlan::DeleteSnapshot,
        (PrunedBy::Retention, DeletionPolicy::Retain) => DeletionPlan::RetainSnapshot,
        (PrunedBy::Retention, DeletionPolicy::Orphan) => DeletionPlan::OrphanSnapshot,
        (PrunedBy::FailedHistory, DeletionPolicy::Delete) => DeletionPlan::DeleteSnapshot,
        (PrunedBy::FailedHistory, DeletionPolicy::Retain) => DeletionPlan::RetainSnapshot,
        (PrunedBy::FailedHistory, DeletionPolicy::Orphan) => DeletionPlan::OrphanSnapshot,
        (PrunedBy::PolicyCascade, DeletionPolicy::Delete) => {
            DeletionPlan::RetainSnapshotOnPolicyDelete
        }
        (PrunedBy::PolicyCascade, DeletionPolicy::Retain) => DeletionPlan::RetainSnapshot,
        (PrunedBy::PolicyCascade, DeletionPolicy::Orphan) => DeletionPlan::OrphanSnapshot,
        // A SnapshotReplication's own retention prune of its dest-side copies:
        // an operator prune exactly like `Retention` (bounded, deliberate,
        // never held). Copy CRs are minted with `deletionPolicy: Delete`, so
        // the `Delete` cell is the one that fires in practice.
        (PrunedBy::ReplicationRetention, DeletionPolicy::Delete) => DeletionPlan::DeleteSnapshot,
        (PrunedBy::ReplicationRetention, DeletionPolicy::Retain) => DeletionPlan::RetainSnapshot,
        (PrunedBy::ReplicationRetention, DeletionPolicy::Orphan) => DeletionPlan::OrphanSnapshot,
        // `concurrencyPolicy: Replace` cancelling an in-flight run. The victim
        // is unfinished by construction (no manifest, so the executor just
        // releases the finalizer); `Retain` on the `Delete` cell is the guard
        // for the race where it committed one after all — never destroy a
        // backup that finished while we were deciding to cancel it.
        (PrunedBy::ReplacedRun, DeletionPolicy::Delete | DeletionPolicy::Retain) => {
            DeletionPlan::RetainSnapshot
        }
        (PrunedBy::ReplacedRun, DeletionPolicy::Orphan) => DeletionPlan::OrphanSnapshot,
    }
}

/// Steps 5-6: external destructive/non-destructive. `Retain`/`Orphan` never
/// contact the repository, so holding them would wedge CR removal for zero
/// protection — only `Delete` consults the breaker.
fn plan_external(policy: DeletionPolicy, breaker: BreakerState) -> DeletionPlan {
    match policy {
        DeletionPolicy::Delete => match breaker {
            BreakerState::Held => DeletionPlan::HoldSnapshotDeletion,
            BreakerState::Allowed => DeletionPlan::DeleteSnapshot,
        },
        DeletionPolicy::Retain => DeletionPlan::RetainSnapshot,
        DeletionPolicy::Orphan => DeletionPlan::OrphanSnapshot,
    }
}

/// The `Snapshot`'s `SnapshotSchedule` controller ownerRef, if any (apiVersion
/// group matches ours, kind == `SnapshotSchedule`).
pub fn schedule_owner_ref(backup: &Snapshot) -> Option<&OwnerReference> {
    backup
        .metadata
        .owner_references
        .as_deref()?
        .iter()
        .find(|o| o.api_version == API_VERSION && o.kind == "SnapshotSchedule")
}

/// Classify the fetched schedule (`None` = 404) against the ownerRef.
/// Present-but-terminating or uid-mismatched ⇒ [`GoneOrReplaced`](OwnerState::GoneOrReplaced).
pub fn owner_state_from(fetched: Option<&SnapshotSchedule>, owner: &OwnerReference) -> OwnerState {
    let Some(sched) = fetched else {
        return OwnerState::GoneOrReplaced;
    };
    let same_uid = sched.meta().uid.as_deref() == Some(owner.uid.as_str());
    let terminating = sched.meta().deletion_timestamp.is_some();
    if same_uid && !terminating {
        OwnerState::Alive
    } else {
        OwnerState::GoneOrReplaced
    }
}

/// Whether this pending deletion counts toward its repository's breaker: a
/// destructive EXTERNAL delete whose plan WITHOUT the breaker is
/// `DeleteSnapshot`. Implemented by re-running [`plan_deletion`] with
/// `breaker = Allowed` — one decision function, no forked logic.
///
/// The `pruned-by` stamp is **exhaustively** classified (no catch-all), because
/// not every stamp is breaker-exempt:
///
/// - `Retention` / `FailedHistory` / `ReplicationRetention` / `ReplacedRun`
///   are OPERATOR prunes — bounded, deliberate, steady-state deletes whose rate
///   is governed elsewhere; they are exempt EVERYWHERE (retention must keep
///   working during an incident, never held).
/// - `PolicyCascade` and unstamped (`None`) are NOT exempt: they fall through to
///   the plan check. A `policy-cascade`-stamped child is quiet-retained in
///   steady state (its plan is `RetainSnapshotOnPolicyDelete`, not
///   `DeleteSnapshot`, so it still doesn't count), but under a namespace
///   teardown with `onNamespaceDelete: Delete` its plan becomes an external
///   destructive `DeleteSnapshot` ([`plan_ns_delete`] → [`plan_external`]) — a
///   mass deletion that only happens because a human deleted a namespace, so it
///   MUST count/hold exactly like an unstamped external child. A new
///   [`PrunedBy`] variant fails to compile until its breaker fate is decided
///   here (ADR §5.5).
pub fn counts_toward_breaker(f: DeletionFacts<'_>) -> bool {
    if !breaker_relevant(pruned_by(f.annotations)) {
        return false;
    }
    matches!(
        plan_deletion(DeletionFacts {
            breaker: BreakerState::Allowed,
            ..f
        }),
        DeletionPlan::DeleteSnapshot
    )
}

/// Whether a `pruned-by` classification is breaker-RELEVANT — a destructive
/// EXTERNAL delete that counts toward / can be held by the mass-deletion breaker
/// — as opposed to an exempt OPERATOR prune. **Exhaustive over [`PrunedBy`]** (no
/// catch-all):
///
/// - `Retention` / `FailedHistory` / `ReplicationRetention` / `ReplacedRun` →
///   `false`: operator prunes, exempt everywhere. `ReplicationRetention` mirrors
///   `Retention` deliberately: a replication's own bounded GFS prune of its
///   copies must keep working during an incident. (Its `mirrorSource` sibling
///   mode stamps NOTHING, so a mass source-vanish classifies EXTERNAL and the
///   dest breaker holds it — that asymmetry is the ransomware guard.)
///   `ReplacedRun` is `concurrencyPolicy: Replace` cancelling this schedule's
///   own still-unfinished run so the newly-due slot can take its place: the
///   user asked for cancel-the-old, and the victim set is bounded by
///   construction (one schedule's unfinished children, at most one slot's worth
///   per fire), so holding it behind the breaker would only wedge the schedule.
/// - `None` (unstamped) / `PolicyCascade` → `true`: breaker-relevant. A
///   `PolicyCascade` member only ever reaches the destructive `DeleteSnapshot`
///   plan (and so the counting set) under an `onNamespaceDelete: Delete` namespace
///   teardown — a mass deletion a human triggered by deleting a namespace, which
///   must count/hold like any external child.
///
/// The single source of truth shared by [`counts_toward_breaker`] and the
/// [`crate::snapshot::batch::PendingMember::external`] flag, so the breaker's
/// count, its held-set, and the surfaced ack value never disagree on which
/// stamps are exempt. A new [`PrunedBy`] variant fails to compile until its
/// breaker relevance is decided here (ADR §5.5).
pub fn breaker_relevant(pruned: Option<PrunedBy>) -> bool {
    match pruned {
        Some(
            PrunedBy::Retention
            | PrunedBy::FailedHistory
            | PrunedBy::ReplicationRetention
            | PrunedBy::ReplacedRun,
        ) => false,
        None | Some(PrunedBy::PolicyCascade) => true,
    }
}

/// Clamp a parsed ack timestamp to `<= now` (clock-skew guard): a future ack
/// value must never pre-acknowledge deletions that haven't happened yet.
pub fn clamp_ack(
    ack: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    ack.map(|a| a.min(now))
}

/// Breaker verdict for one deletion. `threshold == 0` disables the breaker.
/// `ack` is the repo's allow-mass-deletion annotation parsed to UTC and
/// ALREADY CLAMPED by the caller (see [`clamp_ack`]). A deletion is
/// acknowledged iff its `deletionTimestamp <= ack`. Unacked pending count `>=
/// threshold` holds every unacked pending deletion.
pub fn breaker_state(
    unacked_pending_for_repo: usize,
    threshold: u32,
    deletion_timestamp: chrono::DateTime<chrono::Utc>,
    ack: Option<chrono::DateTime<chrono::Utc>>,
) -> BreakerState {
    if threshold == 0 {
        return BreakerState::Allowed;
    }
    if ack.is_some_and(|a| deletion_timestamp <= a) {
        return BreakerState::Allowed;
    }
    if unacked_pending_for_repo >= threshold as usize {
        BreakerState::Held
    } else {
        BreakerState::Allowed
    }
}

/// Whether the mass-deletion breaker's per-repo pending count can be trusted
/// this reconcile. **Fail-safe:** a snapshot reflector store that is unset
/// (`store_present == false`, the `OnceLock` not yet populated at startup) OR
/// not yet synced (`synced == false`, the initial LIST still in flight) must
/// NOT be read as "nothing pending" — the caller requeues instead of computing
/// a count. The two paths behave IDENTICALLY: an absent store and an unsynced
/// store are both "we can't count yet", never "the count is zero".
pub fn breaker_stores_ready(store_present: bool, synced: bool) -> bool {
    store_present && synced
}

/// Parse + clock-skew-clamp the repository's raw `allow-mass-deletion`
/// annotation for the breaker. Returns `(clamped_ack, invalid)`:
///
/// - An absent annotation → `(None, false)`: no ack, nothing to warn about.
/// - A parseable RFC3339 value → `(Some(min(value, now)), false)`: clamped via
///   [`clamp_ack`] so a future-dated ack can't pre-approve deletions that
///   haven't happened yet.
/// - An unparseable non-empty value → `(None, true)`: the ack is IGNORED
///   (fail-safe — a malformed value never disarms the breaker) and `invalid`
///   signals the caller to publish the `InvalidMassDeletionAck` warning.
pub fn parse_mass_deletion_ack(
    raw: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> (Option<chrono::DateTime<chrono::Utc>>, bool) {
    match raw {
        None => (None, false),
        Some(v) => match chrono::DateTime::parse_from_rfc3339(v) {
            Ok(dt) => (clamp_ack(Some(dt.with_timezone(&chrono::Utc)), now), false),
            Err(_) => (None, true),
        },
    }
}

/// Whether the `SnapshotDeletionHeld` Warning event should fire this pass: only
/// on the TRANSITION into held, i.e. the `Snapshot`'s existing
/// [`DELETION_HELD_CONDITION`](crate::consts::DELETION_HELD_CONDITION) is not
/// already `True`. Sourcing `existing` from the freshly re-read conditions
/// (`live_conditions_source`) makes this a real transition detector, so a CR
/// that stays held across requeues emits exactly one event (the Recorder's own
/// aggregation is a backstop, not the primary guard).
pub fn should_emit_held_event(
    existing: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> bool {
    !existing
        .iter()
        .any(|c| c.type_ == crate::consts::DELETION_HELD_CONDITION && c.status == "True")
}

/// The exact `kubectl` command that acknowledges this repository's pending
/// mass-deletion wave, ready to copy from an event/condition message.
/// `ack_value` is the newest pending `deletionTimestamp` for the repo
/// (RFC3339): acknowledging up to it releases every currently-held deletion. The
/// verb/selector is `repository/<name>` (with `-n <ns>`) for a namespaced
/// [`RepositoryKind::Repository`] and `clusterrepository/<name>` (cluster-scoped,
/// no namespace) for a [`RepositoryKind::ClusterRepository`] — exhaustive over
/// the kind so a new repository kind forces a decision here.
pub fn mass_deletion_ack_command(repo: &RepositoryRef, ack_value: &str) -> String {
    let ann = crate::consts::ALLOW_MASS_DELETION_ANNOTATION;
    match repo.kind {
        RepositoryKind::Repository => {
            let ns = repo.namespace.as_deref().unwrap_or_default();
            format!(
                "kubectl -n {ns} annotate repository/{} {ann}=\"{ack_value}\" --overwrite",
                repo.name
            )
        }
        RepositoryKind::ClusterRepository => format!(
            "kubectl annotate clusterrepository/{} {ann}=\"{ack_value}\" --overwrite",
            repo.name
        ),
    }
}

/// The `DeletionHeld=True` condition/event message for a `Snapshot` whose
/// deletion the mass-deletion breaker is holding. Carries what/why/fix: the
/// pending count vs. threshold, the target repository, the EXACT ack command
/// (with the copy-ready value), and the per-CR `skip-snapshot-cleanup` escape
/// hatch. Pure so the surfaced ack value and command are unit-asserted.
pub fn mass_deletion_hold_message(
    repo: &RepositoryRef,
    pending: usize,
    threshold: u32,
    ack_value: &str,
) -> String {
    let kind = match repo.kind {
        RepositoryKind::Repository => "Repository",
        RepositoryKind::ClusterRepository => "ClusterRepository",
    };
    format!(
        "deletion HELD by the mass-deletion breaker: {pending} pending external destructive \
         deletions for {kind} `{}` are at/above its threshold of {threshold}. No kopia data was \
         deleted and this Snapshot keeps its finalizer. Fix: to APPROVE this wave (releases every \
         held deletion for the repository), run: {}. To release THIS Snapshot alone WITHOUT \
         deleting its kopia snapshot, annotate it `{}: \"true\"`.",
        repo.name,
        mass_deletion_ack_command(repo, ack_value),
        SKIP_SNAPSHOT_CLEANUP_ANNOTATION,
    )
}

/// The `SnapshotRetainedOnScheduleDelete` Warning event message for
/// [`DeletionPlan::RetainSnapshotOnScheduleDelete`]'s executor. Names the CR,
/// states that the kopia snapshot was kept, and says how it comes back:
/// rediscovered on the *next catalog scan* (not a promise of "within the
/// refresh interval" — `periodicRefresh` is off by default, so nothing runs on
/// a timer unless the user turned it on; the real triggers are a bootstrap, a
/// spec change, or a recreated policy's automatic scan request), then
/// auto-adopted by default once a matching `SnapshotPolicy` exists. Pure so
/// the wording is unit-tested.
pub fn schedule_cascade_retained_message(namespace: &str, name: &str) -> String {
    format!(
        "Snapshot `{namespace}/{name}` was RETAINED, not deleted: its owning SnapshotSchedule \
         is gone/replaced and the schedule's `onScheduleDelete` is `Retain` (the safe default), \
         so the kopia snapshot is kept even though this Snapshot's deletionPolicy is `Delete`. It \
         will be rediscovered as `origin: discovered` on the next catalog scan (a bootstrap, spec \
         change, or recreated policy's scan request) and auto-adopted once a SnapshotPolicy with \
         a matching identity exists. Fix: to cascade deletes when a schedule is removed, set the \
         schedule's `spec.deletion.onScheduleDelete: Delete`."
    )
}

/// The `SnapshotRetainedOnPolicyDelete` Warning event message for
/// [`DeletionPlan::RetainSnapshotOnPolicyDelete`]'s executor. Names the CR,
/// states what became of the kopia snapshot — retained in the repository, or
/// (`snapshot_recorded == false`) never completed at all because the run was
/// cancelled mid-flight before any kopia snapshot existed — that it (or any
/// future one matching the same identity) will be rediscoverable/adoptable,
/// and the opt-in for users who wanted the cascade to actually delete
/// kopia-side data. Pure so both phrasings are unit-tested.
pub fn policy_cascade_retained_message(
    namespace: &str,
    name: &str,
    snapshot_recorded: bool,
) -> String {
    let outcome = if snapshot_recorded {
        "its kopia snapshot was RETAINED in the repository, not deleted"
    } else {
        "the kopia snapshot for this run was never completed (cancelled mid-flight), so there \
         was nothing in the repository to delete"
    };
    format!(
        "Snapshot `{namespace}/{name}` was released, not deleted: its owning SnapshotPolicy is \
         gone and the policy's `onPolicyDelete` is `Retain` (the safe default), so {outcome}. Any \
         kopia snapshot it created stays rediscoverable/adoptable by a future SnapshotPolicy with \
         a matching identity (catalog scan / auto-adoption). Fix: to cascade deletes when a \
         SnapshotPolicy is removed, set the policy's `spec.deletion.onPolicyDelete: Delete`."
    )
}

/// The `MassDeletionHeld` condition a `Repository`/`ClusterRepository` reconcile
/// upserts (ADR-0005 §6, mirroring `IndexBlobHealth`: non-blocking, alert-only —
/// never flips `Ready`). Pure so the counts→(status/reason/message) mapping is
/// unit-tested for both kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoMassDeletionCondition {
    /// `True` when the breaker is tripping for this repo (held), else `False`.
    pub held: bool,
    /// `ThresholdExceeded` when held, else `BelowThreshold`.
    pub reason: &'static str,
    /// Human message (counts + ack command when held).
    pub message: String,
}

/// Decide the repository's [`RepoMassDeletionCondition`] from its unacked pending
/// count and threshold. Held iff `threshold > 0 && unacked_pending >= threshold`
/// — identical to when [`breaker_state`] would hold each of those deletions.
/// `ack_value` is the newest pending `deletionTimestamp` (RFC3339), surfaced in
/// the held message's ack command; a missing one degrades to `<newest-pending>`
/// placeholder text (never happens while held — a held repo has pending members).
pub fn repo_mass_deletion_condition(
    repo: &RepositoryRef,
    unacked_pending: usize,
    threshold: u32,
    ack_value: Option<&str>,
) -> RepoMassDeletionCondition {
    if threshold > 0 && unacked_pending >= threshold as usize {
        let value = ack_value.unwrap_or("<newest-pending-deletionTimestamp>");
        RepoMassDeletionCondition {
            held: true,
            reason: crate::consts::MASS_DELETION_THRESHOLD_EXCEEDED_REASON,
            message: format!(
                "{unacked_pending} pending external destructive Snapshot deletions for this \
                 repository are at/above the breaker threshold of {threshold}; their finalizers are \
                 HELD until acknowledged. Run: {}.",
                mass_deletion_ack_command(repo, value)
            ),
        }
    } else {
        RepoMassDeletionCondition {
            held: false,
            reason: crate::consts::MASS_DELETION_BELOW_THRESHOLD_REASON,
            message: format!(
                "pending external destructive Snapshot deletions for this repository \
                 ({unacked_pending}) are below the breaker threshold ({threshold})"
            ),
        }
    }
}

/// Effective cascade policy for a `Snapshot`: the stamped value, else `Retain`
/// (covers pre-upgrade + manual + discovered — all safe).
pub fn effective_on_schedule_delete(stamped: Option<ScheduleDeletePolicy>) -> ScheduleDeletePolicy {
    stamped.unwrap_or_default()
}

/// Where a `SnapshotDelete` Job may run. The Kubernetes `NamespaceLifecycle`
/// admission plugin rejects *creating* anything in a terminating namespace, so
/// the namespace-deletion cascade (ADR-0005 §5) can never run its delete Job in
/// the `Snapshot`'s own namespace — it must run where the repository's
/// credentials live, or fall back to orphaning (never wedge the namespace).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteJobPlacement {
    /// Create/poll the delete Job in this (non-terminating) namespace.
    RunIn(String),
    /// No surviving namespace can host the Job — orphan the snapshot instead
    /// (fail-safe: release the finalizer, keep the kopia snapshot, say why).
    OrphanFallback {
        /// Human-readable why + fix, surfaced in the `SnapshotOrphaned` event.
        reason: String,
    },
}

/// Decide where the `SnapshotDelete` Job runs. Pure, so the placement matrix is
/// unit-tested without a cluster:
///
/// - Namespace NOT terminating → the `Snapshot`'s own namespace (status quo).
/// - Terminating + namespaced `Repository` in a *different* namespace → the
///   repository's namespace (its credential Secret and any repo PVC live there).
/// - Terminating + `ClusterRepository` → the operator's namespace (where a
///   `ClusterRepository`'s canonical credential Secret lives, and where its
///   maintenance Jobs already run — ADR §3.7).
/// - Terminating + the repository (or operator) namespace IS the terminating
///   namespace, or the operator namespace is unknown → [`OrphanFallback`]:
///   nothing survivable can host the Job, and an uncreatable Job must not wedge
///   namespace deletion forever.
///
/// [`OrphanFallback`]: DeleteJobPlacement::OrphanFallback
pub fn delete_job_placement(
    ns_terminating: bool,
    snapshot_ns: &str,
    repo_namespace: Option<&str>,
    operator_namespace: Option<&str>,
) -> DeleteJobPlacement {
    if !ns_terminating {
        return DeleteJobPlacement::RunIn(snapshot_ns.to_string());
    }
    match repo_namespace {
        Some(rns) if rns != snapshot_ns => DeleteJobPlacement::RunIn(rns.to_string()),
        Some(_) => DeleteJobPlacement::OrphanFallback {
            reason: format!(
                "the Repository lives in `{snapshot_ns}`, the same namespace being deleted, so no \
                 surviving namespace can host the snapshot-delete Job; the kopia snapshot is \
                 orphaned instead — delete it manually with `kopia snapshot delete` if unwanted"
            ),
        },
        None => match operator_namespace {
            Some(op) if op != snapshot_ns => DeleteJobPlacement::RunIn(op.to_string()),
            Some(op) => DeleteJobPlacement::OrphanFallback {
                reason: format!(
                    "the operator namespace `{op}` is itself the namespace being deleted, so it \
                     cannot host the snapshot-delete Job; the kopia snapshot is orphaned instead"
                ),
            },
            None => DeleteJobPlacement::OrphanFallback {
                reason: "the operator namespace is unknown (KOPIUR_NAMESPACE is unset), so there \
                         is nowhere to run the ClusterRepository snapshot-delete Job during \
                         namespace deletion; set KOPIUR_NAMESPACE on the controller Deployment — \
                         the kopia snapshot is orphaned instead"
                    .to_string(),
            },
        },
    }
}

/// Where a BATCH delete Job runs (mass-deletion protection): always the
/// repository's home namespace (`repo_namespace` for a namespaced `Repository`,
/// `operator_namespace` for a `ClusterRepository`), unless that namespace is
/// itself the terminating one, or the operator namespace is unknown →
/// [`DeleteJobPlacement::OrphanFallback`]. Unlike [`delete_job_placement`],
/// there is no "Snapshot's own namespace" fallback — a batch spans many
/// `Snapshot`s (possibly many namespaces), so it always runs at the
/// repository's home.
pub fn batch_job_placement(
    repo_namespace: Option<&str>,
    operator_namespace: Option<&str>,
    terminating_ns: Option<&str>,
) -> DeleteJobPlacement {
    match repo_namespace {
        Some(rns) if terminating_ns != Some(rns) => DeleteJobPlacement::RunIn(rns.to_string()),
        Some(rns) => DeleteJobPlacement::OrphanFallback {
            reason: format!(
                "the Repository lives in `{rns}`, the same namespace being deleted, so no \
                 surviving namespace can host the snapshot-delete batch Job; the kopia snapshots \
                 are orphaned instead — delete them manually with `kopia snapshot delete` if \
                 unwanted"
            ),
        },
        None => match operator_namespace {
            Some(op) if terminating_ns != Some(op) => DeleteJobPlacement::RunIn(op.to_string()),
            Some(op) => DeleteJobPlacement::OrphanFallback {
                reason: format!(
                    "the operator namespace `{op}` is itself the namespace being deleted, so it \
                     cannot host the snapshot-delete batch Job; the kopia snapshots are orphaned \
                     instead"
                ),
            },
            None => DeleteJobPlacement::OrphanFallback {
                reason: "the operator namespace is unknown (KOPIUR_NAMESPACE is unset), so there \
                         is nowhere to run the ClusterRepository snapshot-delete batch Job; set \
                         KOPIUR_NAMESPACE on the controller Deployment — the kopia snapshots are \
                         orphaned instead"
                    .to_string(),
            },
        },
    }
}

/// Normalize a recipe's `repository` ref for pinning into
/// `status.resolved.repository` (ADR §3.4, frozen at run time): a namespaced
/// `Repository` ref pins the namespace it actually resolved against (the
/// recipe's own namespace when unset) so the deletion path can re-resolve it
/// after the recipe is gone; a `ClusterRepository` ref pins none (the webhook
/// forbids one). Exhaustive over [`RepositoryKind`] (ADR §5.5).
pub fn pinned_repository_ref(r: &RepositoryRef, config_ns: &str) -> RepositoryRef {
    // The one shared normal form (hoisted to the api crate so mint-time pins —
    // schedule fan-out, CLI `snapshot now`, adoption — normalize identically).
    kopiur_api::common::normalized_repository_ref(r, config_ns)
}

pub use crate::naming::capped_name;

/// Build the `status.resolved` body frozen at run time (ADR §3.4): the
/// normalized repository ref ([`pinned_repository_ref`]) plus the concrete
/// source (PVC, when the recipe names one, and the kopia source path the work
/// spec actually snapshots). Pure — unit-tested without a cluster.
pub fn resolved_run_status(
    config: &SnapshotPolicy,
    namespace: &str,
    work_spec: &MoverWorkSpec,
    repo_ref: &RepositoryRef,
) -> kopiur_api::snapshot::ResolvedSnapshot {
    let config_ns = config.namespace().unwrap_or_else(|| namespace.to_string());
    let pvc = config
        .spec
        .sources
        .first()
        .and_then(|s| s.pvc.as_ref())
        .map(|p| format!("{namespace}/{}", p.name));
    kopiur_api::snapshot::ResolvedSnapshot {
        // `repo_ref` is threaded by the caller (pure fn, no error path):
        // multi-repo pin selection lands in M8.
        repository: Some(pinned_repository_ref(repo_ref, &config_ns)),
        sources: vec![kopiur_api::snapshot::ResolvedSource {
            pvc,
            source_path: Some(work_spec.identity.source_path.clone()),
        }],
        credential_projection: Some(projection_to_pin(config)),
    }
}

/// The recipe's credential-projection opt-in, normalized for freezing into
/// `status.resolved` (#255). An absent `spec.credentialProjection` yields an explicit
/// `enabled: false` rather than `None`: the deletion path reads `None` as "this run
/// predates the pin", so a recipe that never opted in must not be indistinguishable
/// from one that was never recorded — otherwise the backfill could never converge and
/// would re-read the recipe on every steady-state pass, forever.
pub fn projection_to_pin(config: &SnapshotPolicy) -> CredentialProjection {
    config
        .spec
        .credential_projection
        .clone()
        .unwrap_or_default()
}

/// The credential-projection opt-in pinned into `status.resolved` at run time.
/// `None` means the `Snapshot` predates the pin (or never ran) — NOT that projection
/// was off; conflating the two is the bug the pin exists to fix (#255).
pub(super) fn pinned_projection(backup: &Snapshot) -> Option<&CredentialProjection> {
    backup
        .status
        .as_ref()?
        .resolved
        .as_ref()?
        .credential_projection
        .as_ref()
}

/// The repository ref pinned into `status.resolved.repository` at run time.
/// `None` means the `Snapshot` predates the pin (pre-#255 fleet) — the
/// mass-deletion breaker's per-repo count treats an unpinned `Snapshot` as
/// conservatively unattributable to any one repository. `pub` (not
/// `pub(super)`, unlike its `pinned_projection` sibling): the deletion
/// gauge observer in `crate::metrics` reads it too.
pub fn pinned_repository(backup: &Snapshot) -> Option<&RepositoryRef> {
    backup
        .status
        .as_ref()?
        .resolved
        .as_ref()?
        .repository
        .as_ref()
}

/// Whether a `Snapshot`'s `status.resolved.repository` pin needs backfilling:
/// the pin is absent AND the `Snapshot` has a `policyRef` to resolve one from.
/// A `Snapshot` with no `policyRef` (discovered/manual)
/// has no recipe to pin from and stays in the conservative unpinned bucket —
/// that is the documented, accepted outcome, not a bug. Pure so the backfill's
/// IO gate (`super::backfill_projection_pin`) is unit-tested without a cluster.
pub(super) fn needs_repository_backfill(backup: &Snapshot) -> bool {
    backup.spec.policy_ref.is_some() && pinned_repository(backup).is_none()
}

/// Build the partial `status.resolved` JSON merge-patch body for
/// `super::backfill_projection_pin`: only the keys that actually need
/// backfilling, so a `Snapshot` needing just one of the two pins never
/// clobbers (or redundantly re-writes) the other. `None` when neither pin
/// needs backfilling — the caller skips the patch entirely.
///
/// `repo_ref` is the row's EFFECTIVE repository
/// ([`kopiur_api::snapshot::effective_repository_ref`] — the spec pin for a
/// multi-repo fan-out child). `None` means the repository is UNKNOWABLE for
/// this row (a pre-feature child of a now-multi-repo policy with no pin):
/// the repository half is then SKIPPED — never guessed — and the row stays in
/// the breaker's conservative unpinned bucket until the policy reconciler's
/// spec-pin backfill (or deletion) resolves it.
pub(super) fn backfill_patch_body(
    config: &SnapshotPolicy,
    namespace: &str,
    needs_projection: bool,
    needs_repository: bool,
    repo_ref: Option<&RepositoryRef>,
) -> Option<serde_json::Value> {
    let repo_backfill = match (needs_repository, repo_ref) {
        (true, Some(r)) => Some(r),
        // Unknowable (multi-repo + unpinned pre-feature row): skip, never guess.
        (true, None) | (false, _) => None,
    };
    if !needs_projection && repo_backfill.is_none() {
        return None;
    }
    let mut resolved = serde_json::Map::new();
    if needs_projection {
        resolved.insert(
            "credentialProjection".to_string(),
            serde_json::json!(projection_to_pin(config)),
        );
    }
    if let Some(repo_ref) = repo_backfill {
        let config_ns = config.namespace().unwrap_or_else(|| namespace.to_string());
        resolved.insert(
            "repository".to_string(),
            serde_json::json!(pinned_repository_ref(repo_ref, &config_ns)),
        );
    }
    Some(serde_json::json!({ "resolved": resolved }))
}

/// Map a `Snapshot` phase to its kstatus [`io::ReadyOutcome`] (ADR-0005 §2), so
/// `kubectl wait --for=condition=Ready` and Flux/Argo health work uniformly. Pure +
/// exhaustive: a new phase cannot compile until its Ready mapping is decided.
///
/// - `Succeeded`/`Discovered`/`Unchanged` → `Ready`. `Unchanged` is Ready for the
///   same reason the others are: the source IS protected. It is covered by the
///   previous snapshot rather than one of its own, which is a fact about which
///   manifest holds the bytes, not about whether the backup worked. Reporting it
///   as anything else would make `kubectl wait --for=condition=Ready` and every
///   Flux/Argo health check fail on a healthy dedupe.
/// - `Failed` → `Stalled` (terminal: won't progress without a spec change/retry).
/// - `Pending`/`Running`/`Deleting` → `Reconciling` (in flight).
pub fn snapshot_ready_outcome(phase: &SnapshotPhase) -> io::ReadyOutcome {
    match phase {
        SnapshotPhase::Succeeded | SnapshotPhase::Discovered | SnapshotPhase::Unchanged => {
            io::ReadyOutcome::Ready
        }
        SnapshotPhase::Failed => io::ReadyOutcome::Stalled,
        SnapshotPhase::Pending | SnapshotPhase::Running | SnapshotPhase::Deleting => {
            io::ReadyOutcome::Reconciling
        }
        // Never `Ready` (a health check must not pass on a phase we cannot read)
        // and never `Stalled` (it may well be progressing under a newer
        // operator): `Reconciling` keeps `kubectl wait` honest.
        SnapshotPhase::Unknown(_) => io::ReadyOutcome::Reconciling,
    }
}

/// What the reconcile body may do for a produced `Snapshot` in `phase`, decided
/// BEFORE the mover Job is consulted. Pure + exhaustive: a new phase cannot
/// compile until its job-creation policy is chosen.
///
/// This is the one-shot discipline the `Restore` reconciler already applies: a
/// Snapshot that reached a terminal phase must NEVER mint another mover Job. The
/// owned Job self-reaps via `ttlSecondsAfterFinished`, and that deletion event
/// re-triggers this reconciler — keying "the work is done" on the Job's
/// *existence* (ephemeral) instead of the phase (durable) re-created the Job and
/// re-ran the whole backup after every TTL reap, forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunDecision {
    /// `Pending`/`Running`/no status yet: drive the mover Job (create or track).
    /// `Running` with a *missing* Job is the resume path — a mid-run Job can only
    /// vanish through outside deletion (the TTL applies after it finishes).
    Run,
    /// `Succeeded`: the kopia snapshot exists. Never touch the Job again; the
    /// only live surfaces are the staged-source reap and `spec.pin` drift.
    SucceededSteadyState,
    /// `Failed`: terminal until the spec changes (ADR: `Failed` → kstatus
    /// `Stalled`); a NEW Snapshot is how a retry happens.
    TerminalFailed,
    /// `Deleting`/`Discovered`: owned by earlier gates (the finalizer path and
    /// the Discovered pin). Reaching the run body in these phases is a watch
    /// desync — wait for a real change rather than acting on stale state.
    ///
    /// Also the landing place for a phase this build cannot interpret: never
    /// launch a mover Job off a phase whose meaning is unknown.
    Wait,
}

/// Decide [`RunDecision`] from the observed phase (see the enum for semantics).
pub fn run_decision(phase: Option<&SnapshotPhase>) -> RunDecision {
    match phase {
        None | Some(SnapshotPhase::Pending) | Some(SnapshotPhase::Running) => RunDecision::Run,
        // `Unchanged` is terminal exactly like `Succeeded` — the mover ran, the
        // source was read, and no further Job may launch for this CR. It takes
        // the same steady-state path so staged-source teardown and the
        // re-issue guard behave identically; the two differ only in what they
        // OWN, which is a status question, not a run-decision one.
        Some(SnapshotPhase::Succeeded | SnapshotPhase::Unchanged) => {
            RunDecision::SucceededSteadyState
        }
        Some(SnapshotPhase::Failed) => RunDecision::TerminalFailed,
        Some(SnapshotPhase::Deleting) | Some(SnapshotPhase::Discovered) => RunDecision::Wait,
        // A phase written by a newer operator: hold. Launching a Job would risk
        // duplicating work that phase already represents, and calling it
        // terminal would strand a run this build simply cannot read.
        Some(SnapshotPhase::Unknown(_)) => RunDecision::Wait,
    }
}

/// Whether the preflight gate should run for a `Snapshot` in `phase`: only at first
/// launch (`None`/`Pending`). A `Running` snapshot whose mover Job vanished resumes
/// via the `run_decision == Run` path; re-evaluating preflight there could demote or
/// fail an in-flight backup on a since-flipped check, so it is excluded.
pub(super) fn should_run_preflight(phase: Option<&SnapshotPhase>) -> bool {
    match phase {
        None | Some(SnapshotPhase::Pending) => true,
        Some(
            SnapshotPhase::Running
            | SnapshotPhase::Succeeded
            | SnapshotPhase::Failed
            | SnapshotPhase::Deleting
            | SnapshotPhase::Discovered
            | SnapshotPhase::Unchanged,
        ) => false,
        // Not knowably "at first launch"; never re-open a preflight gate on a
        // phase this build cannot place in the lifecycle.
        Some(SnapshotPhase::Unknown(_)) => false,
    }
}

/// Whether the repository-pool slot gate should run for a `Snapshot` in
/// `phase`: only at first launch (`None`/`Pending`), exactly like
/// [`should_run_preflight`].
///
/// A `Running` Snapshot whose mover Job vanished takes the resume path
/// (`run_decision == Run`) and is re-admitted UNCONDITIONALLY. Two reasons, and
/// both are about not making a bad situation worse:
///
/// - Demoting `Running` → `Pending` would make a backup that was genuinely in
///   flight look like it never started, and would flap `kubectl wait` and every
///   Flux/Argo health check keyed on the phase.
/// - The run it is resuming already HELD a slot. Re-queuing it behind runs that
///   started later inverts the queue, and — when the pool is full of the very
///   backups that started after it — can hold the resume indefinitely.
///
/// The pin/unpin path is out of the pool entirely
/// ([`crate::pool::counts_toward_repo_pool`]), so it never reaches this gate;
/// the terminal phases below never mint a mover Job at all.
pub(super) fn should_run_pool_gate(phase: Option<&SnapshotPhase>) -> bool {
    match phase {
        None | Some(SnapshotPhase::Pending) => true,
        Some(
            SnapshotPhase::Running
            | SnapshotPhase::Succeeded
            | SnapshotPhase::Failed
            | SnapshotPhase::Deleting
            | SnapshotPhase::Discovered
            | SnapshotPhase::Unchanged,
        ) => false,
        // Never park a run off a phase this build cannot place in the
        // lifecycle: `run_decision` already holds it (`Wait`), so opening a
        // queue gate here could only add a misleading condition.
        Some(SnapshotPhase::Unknown(_)) => false,
    }
}

/// Whether a terminal steady-state pin arm (`pin_discovered_row`/
/// `pin_adopted_row` in [`super`]) needs to patch status this reconcile: the
/// observed phase hasn't already converged to the arm's `target` (`Discovered`
/// for a discovered row, `Succeeded` for an adopted one). Pure + shared, so the
/// "only pin when unset/divergent" idempotence both arms rely on — never
/// re-patching (and so never re-generating kstatus conditions/timestamps) once
/// pinned — is unit-tested without a cluster (M5).
pub(super) fn needs_terminal_pin(observed: Option<&SnapshotPhase>, target: &SnapshotPhase) -> bool {
    observed != Some(target)
}

/// Whether an `Adopted`-resolving row carries CONTROLLER-WRITTEN provenance —
/// `status.snapshot`, the kopia id `adopt_one`'s create→status-patch flow records.
/// [`super::pin_adopted_row`] pins `phase: Succeeded` ONLY when this holds: a
/// user-applied BARE `origin: adopted` label (which `resolve_origin` still resolves
/// to `Adopted` via its label fallback) has no `status.snapshot`, and pinning it
/// would mint a phantom `Succeeded` row that enters GFS retention and sets
/// `has_history`. Pure + shared with the retention-side provenance guards so the
/// "only a real adopted row is Succeeded/history" rule is unit-tested without a
/// cluster. The genuine adopt flow converges within a pass, so an interim row is
/// only transiently phase-less.
pub(super) fn adopted_row_has_provenance(backup: &Snapshot) -> bool {
    backup
        .status
        .as_ref()
        .and_then(|s| s.snapshot.as_ref())
        .is_some()
}

/// Whether the preflight deadline has passed: `preflight_since + timeout <= now`.
/// `timeout == None` ⇒ indefinite (never expires); `preflight_since == None` (the
/// failure just started this reconcile) ⇒ not expired. Pure / clock-injected.
pub(super) fn preflight_expired(
    preflight_since: Option<&str>,
    timeout: Option<std::time::Duration>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let (Some(t), Some(since)) = (
        timeout,
        preflight_since.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()),
    ) else {
        return false;
    };
    let elapsed = now - since.with_timezone(&chrono::Utc);
    elapsed >= chrono::Duration::from_std(t).unwrap_or(chrono::Duration::MAX)
}

/// Build the `(phase, observedGeneration, conditions)` status JSON for a `Snapshot`
/// reaching `phase`, deriving the kstatus Ready/Reconciling/Stalled conditions via
/// [`snapshot_ready_outcome`] + [`io::set_ready`]. Existing conditions (e.g.
/// `CredentialsAvailable`) are preserved by `set_ready`'s upsert.
pub(super) fn snapshot_ready_status(
    backup: &Snapshot,
    phase: SnapshotPhase,
    reason: &str,
    message: &str,
) -> serde_json::Value {
    snapshot_ready_status_over(
        backup,
        &phase,
        reason,
        message,
        &existing_conditions(backup),
    )
}

/// Like [`snapshot_ready_status`], but additionally upserts a domain condition
/// (e.g. `SourceStaged=False`) into the same write, so a terminal transition
/// carries both the specific condition and the derived kstatus set atomically.
pub(super) fn snapshot_ready_status_with_condition(
    backup: &Snapshot,
    phase: SnapshotPhase,
    reason: &str,
    message: &str,
    condition_type: &str,
    condition_status: bool,
) -> serde_json::Value {
    let seeded = io::upsert_condition(
        &existing_conditions(backup),
        condition_type,
        condition_status,
        reason,
        message,
        backup.meta().generation,
    );
    snapshot_ready_status_over(backup, &phase, reason, message, &seeded)
}

/// The status body written when the repository-pool gate PARKS a run: phase
/// `Pending`, `RepositorySlotAvailable=False`/`WaitingForSlot` with `message`,
/// the derived kstatus set — and `status.resolved.repository`, pinned HERE
/// rather than at Job creation.
///
/// **Why stamp `resolved.repository` at park time.** Everything that observes a
/// queued run keys off it: the `kopiur_snapshot_waiting_for_slot` gauge labels
/// its series from it, and it is the only place a `kubectl get -o yaml` of a
/// parked Snapshot says WHICH repository it is queued behind. The ordinary stamp
/// happens in the Job-creation patch, which a parked run by definition never
/// reaches — so without this, a queued backup would be observable only as an
/// unattributed `Pending`.
///
/// **Byte-stability.** The `resolved` key is included ONLY when the pinned ref
/// differs from what status already carries. `io::patch_status_if_changed`
/// compares the keys present in the desired body, and a run that previously
/// stamped the FULL `resolved` block (repository + sources + credentialProjection)
/// would never compare equal to a repository-only body — so unconditionally
/// including it would make every parked pass a real write, bumping
/// `resourceVersion`, re-triggering the primary watch and hot-looping the
/// reconciler for as long as the queue lasts.
pub(super) fn park_status(
    backup: &Snapshot,
    message: &str,
    pinned: &RepositoryRef,
) -> serde_json::Value {
    let mut status = snapshot_ready_status_with_condition(
        backup,
        SnapshotPhase::Pending,
        crate::consts::WAITING_FOR_SLOT_REASON,
        message,
        crate::consts::REPOSITORY_SLOT_AVAILABLE_CONDITION,
        false,
    );
    let already = backup
        .status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .and_then(|r| r.repository.as_ref());
    if already != Some(pinned)
        && let Some(obj) = status.as_object_mut()
    {
        obj.insert(
            "resolved".to_string(),
            serde_json::json!({ "repository": pinned }),
        );
    }
    status
}

fn existing_conditions(
    backup: &Snapshot,
) -> Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    backup
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default()
}

fn snapshot_ready_status_over(
    backup: &Snapshot,
    phase: &SnapshotPhase,
    reason: &str,
    message: &str,
    existing: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> serde_json::Value {
    use kopiur_api::common::PhaseLabel;
    let generation = backup.meta().generation;
    let conditions = io::set_ready(
        existing,
        generation,
        snapshot_ready_outcome(phase),
        reason,
        message,
    );
    serde_json::json!({
        "phase": phase.label(),
        "observedGeneration": generation,
        "conditions": conditions,
    })
}

/// Compute the effective `DeletionPolicy` for a `Snapshot`, honoring the
/// origin-aware default (ADR §4.5): discovered backups are forced to `Retain`,
/// produced backups default to `Delete` when unset.
///
/// `origin` is [`resolve_origin`]'s output: `None` means the row carries an
/// origin marker this build cannot parse — forced `Retain`, exactly like
/// `Discovered`. Conservative in the only direction that matters for backup
/// software: the finalizer of a row we cannot classify must never contact the
/// repository and delete a manifest that may belong to someone else, while
/// `Retain` still releases the CR (never wedges deletion).
pub fn effective_deletion_policy(
    spec_policy: Option<DeletionPolicy>,
    origin: Option<Origin>,
) -> DeletionPolicy {
    match origin {
        // Discovered snapshots are never ours to delete — forced Retain. An
        // UNPARSEABLE origin gets the same treatment (see doc above).
        Some(Origin::Discovered) | None => DeletionPolicy::Retain,
        // Adopted rows are managed like any produced backup: same fallback
        // default. Replicated copies too — the replication run minted the
        // dest-side manifest AND stamps `deletionPolicy: Delete` at create, so
        // the produced-row default is the correct fallback.
        Some(Origin::Scheduled | Origin::Manual | Origin::Adopted | Origin::Replicated) => {
            spec_policy.unwrap_or(DeletionPolicy::Delete)
        }
    }
}

/// The kopia-side pin action a `Snapshot` reconcile must take (ADR-0005 §13(c)),
/// derived purely from `spec.pin` (desired) and `status.pinned` (observed). No IO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinAction {
    /// Apply the pin (`kopia snapshot pin --add`): desired `true`, not yet pinned.
    Pin,
    /// Remove the pin (`kopia snapshot pin --remove`): desired `false`, currently pinned.
    Unpin,
    /// Nothing to do — kopia's pin state already matches `spec.pin`.
    NoOp,
}

/// Decide the kopia-side pin action from the desired (`spec.pin`) and observed
/// (`status.pinned`) state. Pure + exhaustive so the decision is unit-tested and a
/// redundant `kopia snapshot pin` is never issued.
///
/// `observed == None` means we've never reconciled the pin: act iff `desired` is
/// `true` (apply it); a never-pinned snapshot with `desired == false` is already in
/// the right state, so `NoOp` (don't spawn an unpin for a pin that was never set).
pub fn pin_decision(desired: bool, observed: Option<bool>) -> PinAction {
    match (desired, observed) {
        (true, Some(true)) => PinAction::NoOp,
        (true, _) => PinAction::Pin,
        (false, Some(true)) => PinAction::Unpin,
        (false, _) => PinAction::NoOp,
    }
}

/// Resolve a `Snapshot`'s origin from its status (canonical) or its
/// `kopiur.home-operations.com/origin` label, via the total [`Origin::parse`].
///
/// - No marker at all (no `status.origin`, no label) ⇒ `Some(Manual)`: a bare
///   `kubectl create` — unchanged from the pre-parse behavior.
/// - A label that parses ⇒ that origin — unchanged.
/// - A label that does NOT parse ⇒ **`None`**. This is the one behavior
///   change: the old `_ => Origin::Manual` arm classified an unrecognized
///   origin string (a typo, a forged label, or a row written by a NEWER
///   operator during version skew) as `Manual` — routing a foreign row into
///   the backup-run machinery, where `reconcile_inner` mints a mover Job for a
///   snapshot this build does not understand. Every caller must handle `None`
///   conservatively (warn + inert handling — the `Discovered`-shaped
///   direction: never mint a Job, never delete, never retain-count), and must
///   NEVER fold it back to `Manual`.
pub fn resolve_origin(b: &Snapshot) -> Option<Origin> {
    if let Some(o) = b.status.as_ref().and_then(|s| s.origin) {
        return Some(o);
    }
    match b
        .labels()
        .get(crate::consts::ORIGIN_LABEL)
        .map(String::as_str)
    {
        None => Some(Origin::Manual),
        Some(label) => Origin::parse(label),
    }
}
