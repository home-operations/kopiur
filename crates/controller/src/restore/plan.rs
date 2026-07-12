//! Pure decision functions for the `Restore` reconciler.
//!
//! Everything here is a pure function over CR/spec/status values — no `ctx`, no
//! kube IO, no `async` (the in-process kopia-list filters take already-fetched
//! lists). These are the exhaustively-unit-tested decisions the reconcile core
//! in [`super`] wires to the cluster.

use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;

use kopiur_api::{OnMissingSnapshot, Restore, RestorePhase, RestoreSource, RestoreTarget};

use crate::io;

/// Which source mode a restore uses, as a stable string (mirrors
/// `RestoreSource::kind_str`, re-derived through an exhaustive match so a new
/// variant must be handled here too).
pub fn source_mode(source: &RestoreSource) -> &'static str {
    match source {
        RestoreSource::SnapshotRef(_) => "SnapshotRef",
        RestoreSource::FromPolicy(_) => "FromPolicy",
        RestoreSource::Identity(_) => "Identity",
    }
}

/// The default `onMissingSnapshot` for a source mode when the spec doesn't set
/// it (ADR §4.6 / SKILL "Restores fail closed"): `fromPolicy` defaults to
/// `Continue` (deploy-or-restore), everything else fails closed (`Fail`).
pub fn default_on_missing(source: &RestoreSource) -> OnMissingSnapshot {
    match source {
        RestoreSource::FromPolicy(_) => OnMissingSnapshot::Continue,
        RestoreSource::SnapshotRef(_) | RestoreSource::Identity(_) => OnMissingSnapshot::Fail,
    }
}

/// Effective `onMissingSnapshot`: explicit spec value wins, else the per-mode
/// default.
pub fn effective_on_missing(
    spec: Option<OnMissingSnapshot>,
    source: &RestoreSource,
) -> OnMissingSnapshot {
    spec.unwrap_or_else(|| default_on_missing(source))
}

/// State of the passive-populator handshake. Pure model of the §4.7 machine so
/// the reconcile loop can dispatch without re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopulatorState {
    /// `target: populator`: this `Restore` is a passive populator source, awaiting a
    /// PVC `dataSourceRef` to claim it (ADR-0005 §9).
    AwaitingClaim,
    /// An explicit `pvc`/`pvcRef` target: the operator drives the restore directly.
    DirectTarget,
}

/// Wall-clock duration (seconds) of a restore `Job` from its
/// `status.startTime`/`completionTime`. `None` if either is absent or the
/// interval is negative (clock skew). Pure. (`Time.0` is a jiff `Timestamp`.)
pub fn restore_job_duration_seconds(job: &k8s_openapi::api::batch::v1::Job) -> Option<i64> {
    let st = job.status.as_ref()?;
    let start = st.start_time.as_ref()?.0.as_second();
    let end = st.completion_time.as_ref()?.0.as_second();
    let secs = end - start;
    (secs >= 0).then_some(secs)
}

/// Decide the populator state from the restore `target` (ADR-0005 §9). Pure +
/// exhaustive over [`RestoreTarget`] (no `_ =>`), so a new target variant must be
/// considered here before it compiles: `populator` awaits a PVC `dataSourceRef`
/// claim; `pvc`/`pvcRef` is a direct, operator-driven restore.
pub fn populator_state(target: &RestoreTarget) -> PopulatorState {
    match target {
        RestoreTarget::Populator(_) => PopulatorState::AwaitingClaim,
        RestoreTarget::Pvc(_) | RestoreTarget::PvcRef(_) => PopulatorState::DirectTarget,
    }
}

/// Actionable refusal for a populator `Restore` under a namespaced install
/// (what / why / fix — pure so the text is unit-asserted). The populator
/// handshake reads StorageClasses and rebinds PersistentVolumes, both
/// cluster-scoped: a namespaced install's Role RBAC can never grant them, so
/// without this guard the reconcile just wedges on retried 403s while the
/// consumer PVC sits Pending unexplained.
pub fn populator_needs_cluster_scope_message() -> String {
    "target.populator is not available in a namespaced install (installScope: namespaced): \
     the volume-populator handshake reads StorageClasses and rebinds PersistentVolumes, \
     which are cluster-scoped and cannot be granted by the install's Role RBAC. Use \
     target.pvc or target.pvcRef for a direct restore, or reinstall with \
     installScope=cluster to use the populator."
        .to_string()
}
/// Whether `phase` lets the reconcile-entry guard short-circuit. `Failed` always does.
/// `Completed` does for a DIRECT restore (the mover wrote the target PVC itself), but
/// NOT for a populator: there the mover stamps `Completed` on finishing the PRIME PVC
/// while the prime→consumer rebind is still pending, so it must fall through to
/// [`drive_populator_restore`]. Pure.
pub(super) fn phase_is_terminal_at_guard(phase: RestorePhase, state: PopulatorState) -> bool {
    match phase {
        RestorePhase::Failed => true,
        RestorePhase::Completed => state == PopulatorState::DirectTarget,
        RestorePhase::Pending | RestorePhase::Resolving | RestorePhase::Restoring => false,
    }
}

/// True once `pvc` is bound to a `PersistentVolume`. Pure.
pub(super) fn pvc_is_bound(pvc: &PersistentVolumeClaim) -> bool {
    pvc.spec
        .as_ref()
        .and_then(|s| s.volume_name.as_deref())
        .is_some_and(|v| !v.is_empty())
        || pvc.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Bound")
}

/// Where the populator handshake stands for the observed `(claiming PVC, the PV we
/// rebound to it)` pair — the decision that says whether there is anything to populate
/// at all. Pure + exhaustive, so every binding ordering is settled in one unit-tested
/// place and the reconcile loop just `match`es it.
///
/// [`Self::NothingToPopulate`] is the #233 fix. A CSI volume-populator can only hand a
/// volume to an **unbound** claim, so a `Restore` recreated over a claim that is already
/// bound (GitOps prunes and re-applies the CR while the app keeps running on its volume)
/// must NOT provision a prime PVC and restore into it: that prime can never be adopted,
/// and used to sit `Bound` forever holding a full copy of the data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PopulatorHandshake {
    /// The consumer is bound to the PV we rebound to it — the handover landed. Finalize:
    /// restore the PV's original reclaim policy and GC the populate artifacts.
    FinalizeRebound { pv: String },
    /// Our rebind is issued but the consumer has not bound yet — wait for the PV controller.
    AwaitingBind,
    /// Our rebind is issued, but the consumer bound to a **different** PV (a
    /// non-populator-aware provisioner beat us to it): the handover is lost and can never
    /// complete. Reap the populate artifacts and complete. Our PV holds the restored data,
    /// so it is KEPT (forced `Retain`) rather than reclaimed.
    LostRebind { pv: String },
    /// No rebind of ours and the consumer is already bound: nothing to populate. Reap any
    /// leftover populate artifacts and complete as a truthful no-op.
    NothingToPopulate,
    /// The consumer is unbound and no rebind is outstanding: run the populate path.
    Populate,
}

/// Decide the [`PopulatorHandshake`] from the claiming PVC and the PV our populator
/// earmarked for it (`None` ⇒ no rebind of ours is outstanding). Pure.
///
/// [`PopulatorHandshake::LostRebind`] keys on an explicit, **different**
/// `spec.volumeName`. A claim that reads bound only through `status.phase` while our
/// rebind is in flight stays [`PopulatorHandshake::AwaitingBind`]: reading it as a lost
/// rebind would reap a perfectly healthy handover mid-bind.
pub(super) fn populator_handshake(
    consumer: &PersistentVolumeClaim,
    our_rebound_pv: Option<&str>,
) -> PopulatorHandshake {
    let bound_volume = consumer
        .spec
        .as_ref()
        .and_then(|s| s.volume_name.as_deref())
        .filter(|v| !v.is_empty());
    match (our_rebound_pv, bound_volume) {
        (Some(ours), Some(bound)) if bound == ours => PopulatorHandshake::FinalizeRebound {
            pv: ours.to_string(),
        },
        (Some(ours), Some(_)) => PopulatorHandshake::LostRebind {
            pv: ours.to_string(),
        },
        (Some(_), None) => PopulatorHandshake::AwaitingBind,
        (None, _) if pvc_is_bound(consumer) => PopulatorHandshake::NothingToPopulate,
        (None, _) => PopulatorHandshake::Populate,
    }
}

/// True when this `Restore` already completed as an already-bound **no-op**
/// ([`crate::consts::RESTORE_TARGET_ALREADY_BOUND_REASON`] on its `Ready` condition).
///
/// Load-bearing twice over, both times to keep a no-op'd populator quiet and safe:
/// - it suppresses source re-resolution on the 600s heartbeat (a populator's `Completed`
///   is deliberately non-terminal, so the CR keeps reconciling — re-resolving would GET
///   the `SnapshotPolicy`/`Repository` forever, and error-loop the moment a user deletes
///   either); and
/// - it suppresses the `Completed`+unpinned ⟹ "empty" back-fill in
///   [`super::pinned_decision`], which would otherwise durably pin `NoSnapshot` on a
///   deferred-source no-op and make a later, legitimate claim recreation come up EMPTY
///   instead of restoring.
pub(super) fn completed_as_target_already_bound(restore: &Restore) -> bool {
    use crate::consts::{READY_CONDITION, RESTORE_TARGET_ALREADY_BOUND_REASON};
    restore.status.as_ref().is_some_and(|s| {
        s.conditions
            .iter()
            .any(|c| c.type_ == READY_CONDITION && c.reason == RESTORE_TARGET_ALREADY_BOUND_REASON)
    })
}

/// What / why / fix for the already-bound no-op completion (#233). Pure, so the text a
/// human acts on is unit-asserted.
pub(super) fn target_already_bound_message(
    consumer_name: &str,
    bound_volume: Option<&str>,
) -> String {
    let volume = match bound_volume {
        Some(v) => format!("PersistentVolume `{v}`"),
        None => "a PersistentVolume".to_string(),
    };
    format!(
        "populator: the claiming PVC `{consumer_name}` is already bound to {volume}, so there \
         is nothing to populate — the CSI volume-populator handover only applies to an UNBOUND \
         claim. No prime PVC was provisioned and no restore ran; the live volume was not \
         touched. To restore into this claim, delete the PVC and let it be re-created (keeping \
         its dataSourceRef)."
    )
}

/// What / why / fix when our rebind was issued but a DIFFERENT volume won the claim
/// ([`PopulatorHandshake::LostRebind`]). Distinct from [`target_already_bound_message`]
/// because here a prime PVC WAS provisioned and a restore DID run — saying otherwise would
/// hide a full-size `Retain`ed volume the admin now owns. Pure.
pub(super) fn lost_rebind_message(consumer_name: &str, kept_pv: &str) -> String {
    format!(
        "populator: the claiming PVC `{consumer_name}` bound to a DIFFERENT volume than the one \
         this restore prepared for it, so the handover was lost and can never complete — a \
         volume-populator only fills an UNBOUND claim, and something else (a provisioner that \
         ignores dataSourceRef, or an earlier bind) got there first. The restored data is NOT \
         in the claim: it is on PersistentVolume `{kept_pv}`, which was kept (forced \
         reclaimPolicy: Retain) rather than reclaimed. Recover it from there, or delete the \
         claiming PVC and let it be re-created so the restore can run again — and delete \
         `{kept_pv}` once you no longer need it."
    )
}

/// What / why / fix when the claiming PVC was bound out from under a restore that was still
/// RUNNING: some provisioner handed the claim a volume without honoring its `dataSourceRef`,
/// so the populate can never be delivered. Reported as a FAILURE — the app is about to start
/// on that other volume, and calling this a success would tell `kubectl wait` / Flux that a
/// restore landed when it did not. Pure.
pub(super) fn populate_hijacked_message(consumer_name: &str, bound_volume: Option<&str>) -> String {
    let volume = match bound_volume {
        Some(v) => format!("PersistentVolume `{v}`"),
        None => "another PersistentVolume".to_string(),
    };
    format!(
        "populator: the claiming PVC `{consumer_name}` was bound to {volume} while this restore \
         was still writing its prime volume, so the restored data can never reach the claim — \
         the app will come up on whatever that volume holds (an empty one, if a provisioner \
         bound it without honoring the dataSourceRef). This cluster cannot complete a \
         volume-populator handshake for that claim: check that the StorageClass's provisioner \
         supports populators (AnyVolumeDataSource + a populator-aware external-provisioner). \
         The in-flight restore was cancelled and its prime PVC left in place for inspection; a \
         Failed Restore is terminal, so fix the provisioner and create a NEW Restore."
    )
}

/// What / why / fix for reaping populate artifacts that can never be handed over (#233).
/// `artifacts` are pre-described (e.g. ``["prime PVC `prime-abc`"]``) so the note names only
/// what actually existed; `kept_pv` is the PV holding restored data that a lost rebind left
/// behind. Deliberately neutral: this also fires on crash-recovery of a SUCCESSFUL restore
/// (finalize stripped the PV annotation, then the controller died before deleting the prime),
/// where nothing went wrong for the user. Pure.
pub(super) fn reaped_populate_artifacts_note(
    artifacts: &[String],
    consumer_name: &str,
    kept_pv: Option<&str>,
) -> String {
    let mut note = format!(
        "populator: reaped leftover populate artifacts ({}) for the claiming PVC \
         `{consumer_name}`: the claim is already bound, so they could never be handed over to \
         it and the prime volume would otherwise hold a full copy of the restored data forever.",
        artifacts.join(", ")
    );
    if let Some(pv) = kept_pv {
        note.push_str(&format!(
            " PersistentVolume `{pv}` holds the restored data and was KEPT (forced \
             reclaimPolicy: Retain) rather than reclaimed — delete it manually if you do not \
             want it."
        ));
    }
    note
}

/// Map a `Restore` phase to its kstatus [`io::ReadyOutcome`] (ADR-0005 §2), so
/// `kubectl wait --for=condition=Ready` and Flux/Argo health checks work on a
/// `Restore` exactly like every other kopiur CRD. Pure + exhaustive: a new phase
/// cannot compile until its Ready mapping is decided.
///
/// - `Completed` → `Ready` (the restore reached its desired state).
/// - `Failed` → `Stalled` (terminal: a Restore is one-shot; a NEW Restore is how
///   a retry happens).
/// - `Pending`/`Resolving`/`Restoring` → `Reconciling` (in flight).
pub fn restore_ready_outcome(phase: RestorePhase) -> io::ReadyOutcome {
    match phase {
        RestorePhase::Completed => io::ReadyOutcome::Ready,
        RestorePhase::Failed => io::ReadyOutcome::Stalled,
        RestorePhase::Pending | RestorePhase::Resolving | RestorePhase::Restoring => {
            io::ReadyOutcome::Reconciling
        }
    }
}

/// Build the `(phase, observedGeneration, conditions)` status JSON for a `Restore`
/// reaching `phase`, layering the kstatus Ready/Reconciling/Stalled conditions
/// (via [`restore_ready_outcome`] + [`io::set_ready`]) onto `base` — the caller's
/// condition set, normally the Restore's existing conditions plus any domain
/// condition (`Resolved`, `AwaitingClaim`, …) upserted for this transition. Every
/// status write goes through here so domain conditions survive phase writes (a
/// bare `conditions: [..]` array replace used to drop them) and every phase
/// transition carries Ready conditions (the job-success path used to write the
/// phase alone, so `kubectl wait --for=condition=Ready` and Flux healthChecks
/// could never gate on a completed Restore). Mirrors `snapshot_ready_status`.
pub(super) fn restore_ready_status_on(
    restore: &Restore,
    base: &[Condition],
    phase: RestorePhase,
    reason: &str,
    message: &str,
) -> serde_json::Value {
    use kopiur_api::common::PhaseLabel;
    let generation = restore.metadata.generation;
    let conditions = io::set_ready(
        base,
        generation,
        restore_ready_outcome(phase),
        reason,
        message,
    );
    serde_json::json!({
        "phase": phase.label(),
        "observedGeneration": generation,
        "conditions": conditions,
    })
}

/// [`restore_ready_status_on`] over the Restore's existing conditions unchanged —
/// the common case where a transition has no domain condition of its own.
pub(super) fn restore_ready_status(
    restore: &Restore,
    phase: RestorePhase,
    reason: &str,
    message: &str,
) -> serde_json::Value {
    restore_ready_status_on(
        restore,
        &existing_conditions(restore),
        phase,
        reason,
        message,
    )
}

/// True when the kstatus trio on `restore` already reflects `phase`'s outcome,
/// keyed on the one condition that is `True` for that outcome (`Ready` for
/// Completed, `Stalled` for Failed, `Reconciling` for in-flight). Checking the
/// distinctive condition suffices because [`io::set_ready`] always writes the
/// trio together. This is the terminal-gate heal's self-gate: checking the
/// PHASE alone is not enough, because the mover stamps the terminal phase
/// without conditions (so the conditions can still say `Reconciling` — or be
/// absent entirely — while the phase is already `Completed`).
pub(super) fn kstatus_settled_for(restore: &Restore, phase: RestorePhase) -> bool {
    use crate::consts::{READY_CONDITION, RECONCILING_CONDITION, STALLED_CONDITION};
    let distinctive = match restore_ready_outcome(phase) {
        io::ReadyOutcome::Ready => READY_CONDITION,
        io::ReadyOutcome::Stalled => STALLED_CONDITION,
        io::ReadyOutcome::Reconciling => RECONCILING_CONDITION,
    };
    restore.status.as_ref().is_some_and(|s| {
        s.conditions
            .iter()
            .any(|c| c.type_ == distinctive && c.status == "True")
    })
}

/// The Restore's current status conditions (empty when no status yet).
pub(super) fn existing_conditions(restore: &Restore) -> Vec<Condition> {
    restore
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default()
}
/// Where the `waitTimeout` window is anchored: the Restore's creation, or — when a
/// populator RE-OPENED resolution because its claiming PVC was re-created — that moment
/// instead (the `Ready` condition's transition into
/// [`crate::consts::RESTORE_CLAIM_RECREATED_REASON`]).
///
/// Without the re-anchor, a populator that no-op'd months ago and is now asked to populate a
/// freshly re-created claim would measure its wait window from a creation timestamp long
/// past: `wait_remaining_secs` returns `None` on the very first pass, and a `fromPolicy`
/// source (which defaults to `onMissingSnapshot: Continue`) would skip the wait entirely and
/// provision an EMPTY volume the instant the snapshot happens not to be there yet — exactly
/// what the user configured `waitTimeout` to prevent. Pure.
pub(super) fn wait_window_anchor(restore: &Restore, created_epoch: i64) -> i64 {
    use crate::consts::{READY_CONDITION, RESTORE_CLAIM_RECREATED_REASON};
    restore
        .status
        .as_ref()
        .and_then(|s| {
            s.conditions
                .iter()
                .find(|c| c.type_ == READY_CONDITION && c.reason == RESTORE_CLAIM_RECREATED_REASON)
                .map(|c| c.last_transition_time.0.as_second())
        })
        .map_or(created_epoch, |reopened| created_epoch.max(reopened))
}

/// Seconds left in the `waitTimeout` window that started at the Restore's
/// creation, or `None` when no (parseable) window is configured or it has
/// elapsed. Pure, clock-free — unit-tested without a cluster.
pub fn wait_remaining_secs(
    created_epoch: i64,
    wait_timeout: Option<&str>,
    now_epoch: i64,
) -> Option<u64> {
    let timeout = crate::snapshot_schedule::parse_go_duration(wait_timeout?)?;
    let deadline = created_epoch.saturating_add(timeout.as_secs().try_into().ok()?);
    (now_epoch < deadline).then(|| (deadline - now_epoch) as u64)
}
