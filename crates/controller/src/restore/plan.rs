//! Pure decision functions for the `Restore` reconciler.
//!
//! Everything here is a pure function over CR/spec/status values — no `ctx`, no
//! kube IO, no `async` (the in-process kopia-list filters take already-fetched
//! lists). These are the exhaustively-unit-tested decisions the reconcile core
//! in [`super`] wires to the cluster.

use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;

use kopiur_api::{
    OnMissingSnapshot, Restore, RestoreClaimPhase, RestoreClaimStatus, RestorePhase, RestoreSource,
    RestoreTarget,
};

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
    "target.populator is unavailable in a namespaced install (installScope: namespaced): its \
     volume-populator handshake reads StorageClasses and rebinds PersistentVolumes, which are \
     cluster-scoped and cannot be granted by the install's Role RBAC. Fix: use target.pvc or \
     target.pvcRef for a direct restore, or reinstall with installScope=cluster to use the \
     populator."
        .to_string()
}
/// Whether a `Restore` in `phase` has NOT yet launched its mover Job — the set the
/// repository-readiness gate may hold in `Pending` (mirrors the Snapshot
/// reconciler's ordering, where a Job that already exists is tracked to terminal
/// and never re-gated). `Restoring` means a Job is (or moments ago was) live and
/// its outcome must be observed; `Completed`/`Failed` are settled (a populator's
/// non-terminal `Completed` heartbeat must not be flipped back to `Pending` by the
/// gate). Pure + exhaustive over [`RestorePhase`], so a new phase must decide its
/// gate membership before it compiles.
pub(super) fn restore_awaiting_launch(phase: Option<&RestorePhase>) -> bool {
    match phase {
        None | Some(RestorePhase::Pending | RestorePhase::Resolving) => true,
        Some(RestorePhase::Restoring | RestorePhase::Completed | RestorePhase::Failed) => false,
        // Never re-gate a phase this build cannot place in the lifecycle: a
        // newer operator may already have a Job in flight, and flipping the
        // object back to `Pending` would fight it.
        Some(RestorePhase::Unknown(_)) => false,
    }
}

/// Message for a restore held in `Pending` because its repository is not `Ready`
/// (backend unreachable). Mirrors the Snapshot reconciler's
/// `repository_not_ready_message` wording (that helper is module-private to
/// `snapshot` and backup-worded, so the restore carries its own). Pure so the
/// text is unit-asserted.
pub(super) fn repository_not_ready_restore_message(repo_name: &str) -> String {
    format!(
        "waiting for repository `{repo_name}` to become `Ready` before launching the restore \
         (its backend is unreachable); the restore proceeds once the repository reconnects."
    )
}

/// Whether `phase` lets the reconcile-entry guard short-circuit — i.e. whether
/// the whole `Restore` is settled. Pure.
///
/// A DIRECT restore is one-shot: both `Completed` (the mover wrote the target
/// PVC itself) and `Failed` are terminal, and a retry is a NEW `Restore`.
///
/// A POPULATOR short-circuits on NEITHER, and both exceptions are load-bearing:
/// - `Completed` — the mover stamps it on finishing the PRIME PVC while the
///   prime→consumer rebind is still pending, so the pass must fall through to
///   [`super::drive_populator_fanout`] to finish the handover.
/// - `Failed` (#443) — the Restore-level phase is now the AGGREGATE over its
///   claims, and one failed claim must not stop its siblings. Terminality moved
///   to the CLAIM ([`kopiur_api::RestoreClaimPhase::is_terminal`] via
///   [`claim_drive`], which returns [`ClaimDrive::Settled`] for a failed claim
///   under a live claimant), so a failed claim rests exactly as it used to while
///   the rest of the fan-out proceeds.
///
/// **The `Failed` relaxation and [`ClaimReason::artifacts_reapable`] are ONE
/// change and must stay one change.** `super::fail_populate_hijacked` leaves its
/// prime PVC standing because it holds half-written data; what used to protect
/// that prime was precisely this guard refusing to fall through. With the
/// relaxation, the protection is `artifacts_reapable` refusing
/// `PopulateHijacked` in `super::reap_claim_artifacts`. Relax one without the
/// other and a hijacked populate loses its prime — and reports success over it.
pub(super) fn phase_is_terminal_at_guard(phase: &RestorePhase, state: PopulatorState) -> bool {
    match phase {
        RestorePhase::Failed | RestorePhase::Completed => state == PopulatorState::DirectTarget,
        RestorePhase::Pending | RestorePhase::Resolving | RestorePhase::Restoring => false,
        // Not terminal: an uninterpretable phase must not short-circuit the
        // reconcile into "nothing left to do".
        RestorePhase::Unknown(_) => false,
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
        "populator: claiming PVC `{consumer_name}` is already bound to {volume}, so there is \
         nothing to populate — the CSI volume-populator handover only applies to an UNBOUND \
         claim. No prime PVC was provisioned and no restore ran; the live volume was untouched. \
         Fix: to restore into this claim, delete the PVC and let it be re-created (keeping its \
         dataSourceRef)."
    )
}

/// What / why / fix when our rebind was issued but a DIFFERENT volume won the claim
/// ([`PopulatorHandshake::LostRebind`]). Distinct from [`target_already_bound_message`]
/// because here a prime PVC WAS provisioned and a restore DID run — saying otherwise would
/// hide a full-size `Retain`ed volume the admin now owns. Pure.
pub(super) fn lost_rebind_message(consumer_name: &str, kept_pv: &str) -> String {
    format!(
        "populator: claiming PVC `{consumer_name}` bound to a DIFFERENT volume than this restore \
         prepared, so the handover was lost and can never complete — a volume-populator only \
         fills an UNBOUND claim, and something else (a provisioner ignoring dataSourceRef, or an \
         earlier bind) got there first. The restored data is NOT in the claim: it is on \
         PersistentVolume `{kept_pv}`, kept (forced reclaimPolicy: Retain). Fix: recover it from \
         there, or delete the claiming PVC to let the restore re-run — then delete `{kept_pv}` \
         when done."
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
        "populator: claiming PVC `{consumer_name}` was bound to {volume} while this restore was \
         still writing its prime volume, so the restored data can never reach the claim — the \
         app will come up on whatever that volume holds (empty, if a provisioner bound it \
         ignoring the dataSourceRef). This cluster cannot complete the volume-populator \
         handshake: check the StorageClass provisioner supports populators (AnyVolumeDataSource \
         + a populator-aware external-provisioner). The in-flight restore was cancelled and its \
         prime PVC left for inspection; a Failed Restore is terminal, so fix the provisioner and \
         create a NEW Restore."
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
        "populator: reaped leftover populate artifacts ({}) for claiming PVC `{consumer_name}`: \
         the claim is already bound, so they could never be handed over and the prime volume \
         would otherwise hold a full copy of the restored data forever.",
        artifacts.join(", ")
    );
    if let Some(pv) = kept_pv {
        note.push_str(&format!(
            " PersistentVolume `{pv}` holds the restored data and was KEPT (forced \
             reclaimPolicy: Retain) — delete it manually if you do not want it."
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
pub fn restore_ready_outcome(phase: &RestorePhase) -> io::ReadyOutcome {
    match phase {
        RestorePhase::Completed => io::ReadyOutcome::Ready,
        RestorePhase::Failed => io::ReadyOutcome::Stalled,
        RestorePhase::Pending | RestorePhase::Resolving | RestorePhase::Restoring => {
            io::ReadyOutcome::Reconciling
        }
        // Never `Ready` and never `Stalled` — `kubectl wait` keeps waiting
        // rather than passing or failing on a phase we cannot read.
        RestorePhase::Unknown(_) => io::ReadyOutcome::Reconciling,
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
        restore_ready_outcome(&phase),
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
pub(super) fn kstatus_settled_for(restore: &Restore, phase: &RestorePhase) -> bool {
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

/// What the repository-readiness gate could learn about the repository a restore
/// will connect to (issue #393) — the return of `super::restore_repository_ref`.
///
/// The point of the enum is that "no repository ref" is THREE different
/// situations that the pre-#393 `Option<(RepositoryRef, String)>` collapsed into
/// one `None`, and only some of them may fall through the gate unverified:
///
/// - [`Self::Derived`] — the ref is known; the gate goes on to check readiness.
/// - [`Self::SnapshotRowMissing`] — a `snapshotRef` whose `Snapshot` CR does not
///   exist (yet). Deliberately falls through: the `waitTimeout` window exists
///   precisely to wait for that row, and `onMissingSnapshot: Fail` must be able
///   to fire for a typo'd ref. Parking here would break both.
/// - [`Self::ReferentMissing`] — a `fromPolicy` whose `SnapshotPolicy` does not
///   exist. Nothing downstream can wait for it usefully, and letting the gate
///   fall through stamps `status.waitStartedAt` against a repository nobody
///   verified — the #393 bug.
/// - [`Self::NotDerivable`] — the referent EXISTS but names no single repository
///   (a `Snapshot` with neither pin nor repository owner, a multi-repository
///   `fromPolicy` with no explicit selection, a raw `identity` source). That is a
///   spec problem, not a missing object: it falls through to the downstream
///   validation that fails closed and lists the valid choices. Parking would
///   hide a permanent misconfiguration behind a "waiting…" message.
///
/// **Match this enum EXHAUSTIVELY.** Never `matches!(…)` it and never add a
/// `_ =>` arm: the compiler's exhaustiveness check is the only thing that makes
/// a fifth shape decide, at compile time, whether it may spend a restore's wait
/// window. This is a COMPILER-ONLY guard: `cargo xtask check-phases` scans only
/// the `*Phase` enums in `kopiur-api`, so nothing but exhaustiveness protects it
/// — which is exactly why the rule is written down here (same convention as the
/// `unreadable_phase` named predicate in `repository_replication`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RepoRefLookup {
    /// The repository ref, plus the namespace it resolves relative to.
    Derived(kopiur_api::common::RepositoryRef, String),
    /// `snapshotRef` naming a `Snapshot` CR that does not exist — the shape the
    /// `waitTimeout` window is FOR. The gate must not engage.
    SnapshotRowMissing,
    /// A referent object the repository ref is derived FROM does not exist.
    /// `namespace` is `None` for a cluster-scoped referent.
    ReferentMissing {
        /// The referent's kind, for the message (`SnapshotPolicy`, …).
        kind: &'static str,
        /// The namespace it was looked up in; `None` when cluster-scoped.
        namespace: Option<String>,
        /// The referent's name.
        name: String,
    },
    /// The referent exists but yields no single repository ref (or there is
    /// nothing to derive from at all): a spec problem for downstream validation.
    NotDerivable,
}

/// Classify a `snapshotRef` lookup: the fetched `Snapshot` (`None` ⇒ the row
/// does not exist) into a [`RepoRefLookup`]. Pure.
///
/// A missing ROW is [`RepoRefLookup::SnapshotRowMissing`] — a supported,
/// waited-for shape. A row that exists but carries no derivable repository
/// (neither `status.resolved.repository`, nor `spec.repository`, nor a
/// repository `ownerReference`) is [`RepoRefLookup::NotDerivable`]: the object
/// is there, so nothing will appear later to fix it.
pub(super) fn classify_snapshot_lookup(
    snapshot: Option<&kopiur_api::Snapshot>,
    snapshot_namespace: &str,
) -> RepoRefLookup {
    match snapshot {
        None => RepoRefLookup::SnapshotRowMissing,
        Some(snap) => match kopiur_api::snapshot::repository_ref_for(snap) {
            Some(rref) => RepoRefLookup::Derived(rref, snapshot_namespace.to_string()),
            None => RepoRefLookup::NotDerivable,
        },
    }
}

/// Classify a `fromPolicy` lookup: the fetched `SnapshotPolicy` (`None` ⇒ the
/// object does not exist) into a [`RepoRefLookup`]. Pure.
///
/// A missing POLICY is [`RepoRefLookup::ReferentMissing`] — the gate parks. A
/// policy that exists but fans out over several repositories with no explicit
/// selection is [`RepoRefLookup::NotDerivable`]: the gate cannot know which
/// repository to wait on and must never guess repository #1, so it falls through
/// to `resolve_restore_repository`, which fails closed listing the valid choices.
pub(super) fn classify_policy_lookup(
    policy: Option<&kopiur_api::SnapshotPolicy>,
    policy_namespace: &str,
    policy_name: &str,
) -> RepoRefLookup {
    match policy {
        None => RepoRefLookup::ReferentMissing {
            kind: "SnapshotPolicy",
            namespace: Some(policy_namespace.to_string()),
            name: policy_name.to_string(),
        },
        Some(cfg) => match kopiur_api::single_repository_ref(&cfg.spec) {
            Ok(rref) => RepoRefLookup::Derived(rref.clone(), policy_namespace.to_string()),
            Err(_) => RepoRefLookup::NotDerivable,
        },
    }
}

/// Message for a restore parked because a referent it derives its repository
/// from does not exist (issue #393). Pure so the text is unit-asserted.
///
/// Says what is missing (kind + namespaced name), why that blocks the restore,
/// that the `waitTimeout` window is deliberately NOT running meanwhile, and how
/// to clear it.
pub(super) fn referent_missing_restore_message(
    kind: &str,
    namespace: Option<&str>,
    name: &str,
) -> String {
    let target = match namespace {
        Some(ns) => format!("{ns}/{name}"),
        None => name.to_string(),
    };
    format!(
        "waiting for {kind} `{target}` to exist: the repository this restore connects to is \
         derived from it, so kopiur cannot verify the backend is reachable and will not launch \
         the restore. The policy.waitTimeout window is NOT running meanwhile \
         (status.waitStartedAt stays unstamped) — it opens once the {kind} exists and its \
         repository is `Ready`. Create the {kind} (or repoint the Restore at one that exists) \
         to proceed."
    )
}

/// A stale [`crate::consts::RESTORE_REFERENT_AVAILABLE_CONDITION`] = `False`
/// left by an earlier park, flipped back to `True` — or `None` when there is
/// nothing to clear (the healthy wire never grows the condition). Pure.
///
/// Without this the park's gate condition outlives the park: the registry row is
/// age-independent, so `kubectl kopiur doctor` would keep reporting a restore
/// that has long since proceeded as blocked on a referent that has existed for
/// hours. Mirrors how the `MoverPermitted`/`CredentialsAvailable` gates clear.
pub(super) fn cleared_referent_conditions(restore: &Restore) -> Option<Vec<Condition>> {
    use crate::consts::{RESTORE_REFERENT_AVAILABLE_CONDITION, RESTORE_REFERENT_FOUND_REASON};
    let existing = existing_conditions(restore);
    if !existing
        .iter()
        .any(|c| c.type_ == RESTORE_REFERENT_AVAILABLE_CONDITION && c.status != "True")
    {
        return None;
    }
    Some(io::upsert_condition(
        &existing,
        RESTORE_REFERENT_AVAILABLE_CONDITION,
        true,
        RESTORE_REFERENT_FOUND_REASON,
        "the referent the restore derives its repository from now exists",
        restore.metadata.generation,
    ))
}

/// `restore` with `conditions` substituted — the in-memory mirror of a status
/// patch this pass has ALREADY made, so the rest of the pass builds on what the
/// server now holds instead of the reconcile-start copy. Pure.
///
/// This is what keeps [`cleared_referent_conditions`] from starting a write
/// loop. Nearly every condition writer downstream of the readiness gate rebuilds
/// the array from the `restore` it was handed (a merge patch replaces the array
/// wholesale, so it has to), and four of them — the `MissingCaBundle`,
/// `MissingServiceAccount`, `PrivilegedMover` and `MissingCredentials` gate parks
/// in `run_restore_mover` — patch UNCONDITIONALLY. Against a reconcile-start copy
/// that still carries `ReferentAvailable=False`, a clear written earlier in the
/// same pass would be re-written back to `False` by that park, then cleared again
/// next pass: two resourceVersion-bumping writes per iteration, each waking the
/// watch, forever — for exactly the GitOps bring-up (repository up, credentials
/// Secret not yet) this feature exists to serve. Those parks are byte-identical
/// no-ops today ONLY because nothing writes conditions before them; carrying the
/// cleared copy forward preserves that.
pub(super) fn restore_with_conditions(restore: &Restore, conditions: Vec<Condition>) -> Restore {
    let mut carried = restore.clone();
    let mut status = carried.status.take().unwrap_or_default();
    status.conditions = conditions;
    carried.status = Some(status);
    carried
}

/// The `Restore` the rest of a reconcile pass must build on: the one the
/// reconcile was handed, or — when the readiness gate cleared a stale
/// `ReferentAvailable=False` — an owned copy carrying the cleared conditions.
///
/// A hand-written two-variant carrier rather than `Cow<'a, Restore>`, for one
/// concrete reason: `Restore` is a large struct, so a `Cow` stores the owned
/// variant INLINE and every value of the enum carrying it (here
/// [`super::RepositoryGate`], whose other variants are one `Action` each) grows
/// to that size — `clippy::large_enum_variant`, denied workspace-wide. Boxing
/// the whole `Cow` (clippy's suggestion) would allocate on the borrowed path
/// too, which is the overwhelmingly common one and must stay free. Boxing only
/// [`Self::Cleared`] keeps the carrier pointer-sized in both arms: the unchanged
/// path is still a bare borrow with no clone and no allocation, and the cleared
/// path pays one `Box` on top of a `Restore` clone it was already making.
///
/// [`Self::get`] is an exhaustive `match`, so a third carrier shape would have
/// to say which object the pass continues with.
#[derive(Debug)]
pub(super) enum CarriedRestore<'a> {
    /// Nothing was cleared: the reconcile's own `Restore`, borrowed.
    Unchanged(&'a Restore),
    /// The gate cleared a stale gate condition: this copy mirrors the status
    /// patch it just made, and is what every later writer must rebuild from.
    Cleared(Box<Restore>),
}

impl CarriedRestore<'_> {
    /// The `Restore` to continue the pass with. Exhaustive.
    pub(super) fn get(&self) -> &Restore {
        match self {
            Self::Unchanged(restore) => restore,
            Self::Cleared(restore) => restore,
        }
    }
}

/// The [`CarriedRestore`] for this pass, given what
/// [`cleared_referent_conditions`] produced: the original borrowed when nothing
/// was cleared (the overwhelmingly common path — no clone), an owned copy
/// carrying the cleared conditions when something was. Pure and TOTAL.
///
/// The single construction site for the readiness gate's proceed payload, so the
/// "cleared but continued from the stale copy" combination — the one that starts
/// the alternating-write loop described on [`restore_with_conditions`] — has no
/// place to be written. Both arms are unit-asserted.
pub(super) fn carried_after_clear<'a>(
    restore: &'a Restore,
    cleared: Option<&[Condition]>,
) -> CarriedRestore<'a> {
    match cleared {
        None => CarriedRestore::Unchanged(restore),
        Some(conditions) => CarriedRestore::Cleared(Box::new(restore_with_conditions(
            restore,
            conditions.to_vec(),
        ))),
    }
}

/// Where the `waitTimeout` window is anchored: `status.waitStartedAt` once
/// [`super::ensure_wait_anchor`] has stamped it, else `created_epoch`.
///
/// The window opens when the restore can first actually PROCEED — its repository is
/// `Ready` and, for a `target.populator`, a PVC already claims it — not when the
/// Restore object happened to be created (#380). A standing GitOps `Restore` applied
/// long before its repository comes up (or long before anything claims it) would
/// otherwise spend the whole window parked on the readiness gate, and a `fromPolicy`
/// source — which defaults to `onMissingSnapshot: Continue` — would provision an EMPTY
/// volume on the very first pass that reaches resolution, exactly what `waitTimeout`
/// was configured to prevent.
///
/// The stamp can never SHORTEN the window below the creation-anchored one
/// (`created_epoch.max(..)`), so an anchor edited backwards by hand is inert: this
/// change is one-directional, windows only ever extend. Pure — the stamping (and the
/// "not open yet" case, which anchors at `now` rather than at creation) lives in
/// [`super::ensure_wait_anchor`].
pub(super) fn effective_wait_anchor(restore: &Restore, created_epoch: i64) -> i64 {
    restore
        .status
        .as_ref()
        .and_then(|s| s.wait_started_at.as_deref())
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map_or(created_epoch, |at| created_epoch.max(at.timestamp()))
}

/// Where a Restore's `waitTimeout` window stands on this pass — the return of
/// [`super::ensure_wait_anchor`], carried to the one place that parks on it. Closed so the
/// "not open yet" state can never be silently reported as an ordinary snapshot wait: the
/// two park with different messages AND different cadences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitWindow {
    /// Open, anchored at this epoch — `status.waitStartedAt` once stamped, else the
    /// Restore's creation (which is also what an unconfigured window reads, where nothing
    /// measures the anchor at all).
    Open(i64),
    /// Not opened: a `target.populator` with no PVC claiming it cannot proceed, so its
    /// window has not started. Anchored at `now`, so the full window always remains.
    AwaitingClaim(i64),
}

impl WaitWindow {
    /// The epoch this pass measures `waitTimeout` from.
    pub fn anchor(self) -> i64 {
        match self {
            Self::Open(at) | Self::AwaitingClaim(at) => at,
        }
    }
}

/// How to park a restore that is still inside (or has not yet started) its wait: the
/// condition `reason`, the actionable message, and the requeue cadence in seconds.
///
/// Exhaustive over [`WaitWindow`] because the two states are NOT interchangeable. An
/// unclaimed populator is the state that most often reaches here — resolution runs while a
/// populator is `AwaitingClaim` — and reporting it as an ordinary snapshot wait would point
/// the user at `status.waitStartedAt`, which is deliberately absent until a claim appears.
/// It also never resolves on its own, so it takes the awaiting-claim cadence (30s) rather
/// than the wait cadence (≤15s, and never past the deadline). Pure.
pub(super) fn wait_park_report(
    window: WaitWindow,
    wait_timeout: Option<&str>,
    remaining: u64,
) -> (&'static str, String, u64) {
    match window {
        WaitWindow::Open(_) => (
            crate::consts::WAITING_FOR_SNAPSHOT_REASON,
            format!(
                "no snapshot matched the restore source yet; waiting up to waitTimeout \
                 ({}) from when the wait window opened (status.waitStartedAt) for it to \
                 appear before applying onMissingSnapshot",
                wait_timeout.unwrap_or_default()
            ),
            remaining.clamp(1, 15),
        ),
        WaitWindow::AwaitingClaim(_) => (
            crate::consts::AWAITING_PVC_DATA_SOURCE_REF_REASON,
            "passive populator: no PersistentVolumeClaim claims this Restore yet \
             (spec.dataSourceRef), so there is nothing to populate and the waitTimeout \
             window has NOT started — it opens when a claim appears, and \
             status.waitStartedAt records that instant. Create the claiming PVC to proceed."
                .to_string(),
            30,
        ),
    }
}

/// Whether the `waitTimeout` window may OPEN on this pass — i.e. whether the restore can
/// actually proceed now that its repository is `Ready`. A direct target always can; a
/// `target.populator` only once a PVC claims it, because resolution runs while the
/// populator is `AwaitingClaim` and a standing GitOps populator would otherwise burn its
/// whole window sitting idle (#380). Exhaustive over [`PopulatorState`], so a new target
/// mode must decide this before it compiles. Pure.
pub(super) fn wait_window_opens(state: PopulatorState, has_claiming_pvc: bool) -> bool {
    match state {
        PopulatorState::DirectTarget => true,
        PopulatorState::AwaitingClaim => has_claiming_pvc,
    }
}

/// The absolute instant (RFC3339) the `waitTimeout` window closes, given the anchor
/// [`effective_wait_anchor`] resolved — `None` when no (parseable) window is
/// configured. This is what the mover polls against, so an in-Job wait is stable
/// across pod retries and matches the controller-side wait exactly. Pure.
pub(super) fn wait_deadline_rfc3339(
    anchor_epoch: i64,
    wait_timeout: Option<&str>,
) -> Option<String> {
    let timeout = crate::snapshot_schedule::parse_go_duration(wait_timeout?)?;
    let secs = i64::try_from(timeout.as_secs()).ok()?;
    chrono::DateTime::from_timestamp(anchor_epoch.checked_add(secs)?, 0).map(|t| t.to_rfc3339())
}

/// Seconds left in the `waitTimeout` window that opened at `anchor_epoch` (see
/// [`effective_wait_anchor`]), or `None` when no (parseable) window is configured or it
/// has elapsed. Pure, clock-free — unit-tested without a cluster.
pub fn wait_remaining_secs(
    anchor_epoch: i64,
    wait_timeout: Option<&str>,
    now_epoch: i64,
) -> Option<u64> {
    let timeout = crate::snapshot_schedule::parse_go_duration(wait_timeout?)?;
    let deadline = anchor_epoch.saturating_add(timeout.as_secs().try_into().ok()?);
    (now_epoch < deadline).then(|| (deadline - now_epoch) as u64)
}

// --- #443: the fanned-out populator's per-claim decisions -------------------
//
// A populator `Restore` is claimed by EVERY PVC whose `spec.dataSourceRef` names
// it. Each claim gets its own prime PVC, mover `Job`, source path and record in
// `status.claims`; the Restore's own phase is the AGGREGATE over those records.
// Everything below is pure over the records alone, so the whole fan-out state
// machine is unit-tested without a cluster.

/// How one claim's phase counts toward the aggregate. Pure + exhaustive over
/// [`RestoreClaimPhase`], so a new claim phase has to declare which side of
/// "still working / settled / failed" it lands on before it compiles — the
/// classification the Restore's own phase, its kstatus trio and the reaper all
/// rest on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimClass {
    /// Not settled: still waiting, populating, or rebinding.
    InFlight,
    /// The claim is bound to the volume this restore populated.
    Populated,
    /// The claim was already bound, so there was nothing to populate (#233).
    AlreadyBound,
    /// The claim terminally failed.
    Failed,
}

/// Classify one claim record's phase. An absent phase is a record the controller
/// has observed but not yet driven — in flight, never settled.
pub fn claim_class(phase: Option<&RestoreClaimPhase>) -> ClaimClass {
    match phase {
        None
        | Some(
            RestoreClaimPhase::Pending
            | RestoreClaimPhase::Populating
            | RestoreClaimPhase::Rebinding,
        ) => ClaimClass::InFlight,
        Some(RestoreClaimPhase::Populated) => ClaimClass::Populated,
        Some(RestoreClaimPhase::AlreadyBound) => ClaimClass::AlreadyBound,
        Some(RestoreClaimPhase::Failed) => ClaimClass::Failed,
        // A phase this build cannot read is never reported as settled: a newer
        // operator may still be driving it, and calling the Restore `Completed`
        // over it would tell `kubectl wait`/Flux a restore landed that did not.
        Some(RestoreClaimPhase::Unknown(_)) => ClaimClass::InFlight,
    }
}

/// The counted shape of a claims map — what [`claims_summary`] renders and what
/// [`aggregate_phase`] decides from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClaimTally {
    /// How many claims there are in total.
    pub total: usize,
    /// How many are `Populated`.
    pub populated: usize,
    /// How many were already bound (a truthful no-op, #233).
    pub already_bound: usize,
    /// Names of the claims not settled yet, in map (name) order.
    pub in_flight: Vec<String>,
    /// Of those, how many have a mover actually running (`Populating`) or a
    /// rebind outstanding (`Rebinding`) — the difference between the Restore
    /// reporting `Restoring` and reporting `Pending`.
    pub active: usize,
    /// `(claim, reason)` for each failed claim, in map order.
    pub failed: Vec<(String, String)>,
}

/// Where a fanned-out populator stands, over all of its claims.
///
/// Closed and matched exhaustively because the four states drive different
/// phases, different cadences and different work: [`Self::NoClaims`] is the
/// standing GitOps populator nothing has claimed yet (its wait window has not
/// even opened); [`Self::Failed`] stalls the Restore while siblings keep going;
/// [`Self::AllSettled`] is the only state that may skip the PV LIST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimsAggregate {
    /// No PVC claims this populator yet.
    NoClaims,
    /// At least one claim terminally failed.
    Failed(ClaimTally),
    /// Nothing failed and at least one claim is still working.
    InFlight(ClaimTally),
    /// Every claim reached a terminal state and none failed.
    AllSettled(ClaimTally),
}

impl ClaimsAggregate {
    /// The tally behind this aggregate; `None` only for [`Self::NoClaims`].
    /// Exhaustive.
    pub fn tally(&self) -> Option<&ClaimTally> {
        match self {
            Self::NoClaims => None,
            Self::Failed(t) | Self::InFlight(t) | Self::AllSettled(t) => Some(t),
        }
    }
}

/// Fold the per-claim records into the whole restore's state. Pure.
pub fn aggregate_claims(
    claims: &std::collections::BTreeMap<String, RestoreClaimStatus>,
) -> ClaimsAggregate {
    if claims.is_empty() {
        return ClaimsAggregate::NoClaims;
    }
    let mut tally = ClaimTally {
        total: claims.len(),
        ..Default::default()
    };
    for (name, record) in claims {
        match claim_class(record.phase.as_ref()) {
            ClaimClass::InFlight => {
                tally.in_flight.push(name.clone());
                if claim_is_active(record.phase.as_ref()) {
                    tally.active += 1;
                }
            }
            ClaimClass::Populated => tally.populated += 1,
            ClaimClass::AlreadyBound => tally.already_bound += 1,
            ClaimClass::Failed => tally.failed.push((
                name.clone(),
                record
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Failed".to_string()),
            )),
        }
    }
    if !tally.failed.is_empty() {
        return ClaimsAggregate::Failed(tally);
    }
    if !tally.in_flight.is_empty() {
        return ClaimsAggregate::InFlight(tally);
    }
    ClaimsAggregate::AllSettled(tally)
}

/// Whether this claim has work actually in flight (a mover running or a rebind
/// outstanding), as opposed to merely waiting. Pure + exhaustive.
fn claim_is_active(phase: Option<&RestoreClaimPhase>) -> bool {
    match phase {
        Some(RestoreClaimPhase::Populating | RestoreClaimPhase::Rebinding) => true,
        None
        | Some(
            RestoreClaimPhase::Pending
            | RestoreClaimPhase::Populated
            | RestoreClaimPhase::AlreadyBound
            | RestoreClaimPhase::Failed,
        ) => false,
        // Never claimed as active: an unreadable phase must not make the Restore
        // report `Restoring` when nothing of ours is running.
        Some(RestoreClaimPhase::Unknown(_)) => false,
    }
}

/// The `Restore`'s own phase, given where its claims stand. Pure + exhaustive
/// over [`ClaimsAggregate`], so kstatus (via the unchanged
/// [`restore_ready_outcome`]) follows automatically.
///
/// `InFlight` splits: `Restoring` once a mover is actually running or a rebind is
/// outstanding, `Pending` while every unsettled claim is merely waiting (for a
/// scheduling hint, the repository, or the source snapshot) — the same
/// distinction the single-claim path drew before #443.
pub fn aggregate_phase(aggregate: &ClaimsAggregate) -> RestorePhase {
    // Every payload is BOUND, never `(_)`: an arm-initial wildcard in a match
    // that yields a phase is exactly the shape `cargo xtask check-phases` rule B
    // exists to catch (#351's `_ => continue` over `SnapshotPhase::Unchanged`).
    match aggregate {
        ClaimsAggregate::NoClaims => RestorePhase::Pending,
        ClaimsAggregate::Failed(failed) => {
            debug_assert!(!failed.failed.is_empty());
            RestorePhase::Failed
        }
        ClaimsAggregate::InFlight(tally) if tally.active > 0 => RestorePhase::Restoring,
        ClaimsAggregate::InFlight(waiting) => {
            debug_assert_eq!(waiting.active, 0);
            RestorePhase::Pending
        }
        ClaimsAggregate::AllSettled(settled) => {
            debug_assert!(settled.in_flight.is_empty() && settled.failed.is_empty());
            RestorePhase::Completed
        }
    }
}

/// The `(reason, message)` for the Restore's Ready/Stalled condition, given the
/// aggregate. Pure so the text a human acts on is unit-asserted.
///
/// The reason is a STABLE string per aggregate state, never the failing claim's
/// own reason: the condition would otherwise churn (and re-bump
/// `resourceVersion`) every time a different claim failed. Per-claim detail goes
/// in the message, and in `status.claims`.
pub fn claims_summary(aggregate: &ClaimsAggregate) -> (&'static str, String) {
    use crate::consts::{
        AWAITING_PVC_DATA_SOURCE_REF_REASON, POPULATING_PRIME_PVC_REASON,
        RESTORE_CLAIM_FAILED_REASON, RESTORE_POPULATED_REASON,
    };
    let Some(tally) = aggregate.tally() else {
        return (
            AWAITING_PVC_DATA_SOURCE_REF_REASON,
            "passive populator: no PersistentVolumeClaim claims this Restore yet \
             (spec.dataSourceRef), so there is nothing to populate and the waitTimeout window \
             has NOT started — it opens when a claim appears. Create the claiming PVC to \
             proceed."
                .to_string(),
        );
    };
    // "settled", not "populated": `already_bound` claims are settled successes
    // that populated nothing (#233), so counting only `populated` against the
    // total renders a healthy all-no-op restore as "0/1" beside `Ready=True`,
    // which reads as a contradiction. The breakdown follows on the same line.
    let settled = tally.populated + tally.already_bound;
    let mut message = format!("{}/{} claims settled", settled, tally.total);
    if tally.populated > 0 {
        message.push_str(&format!("; populated: {}", tally.populated));
    }
    if tally.already_bound > 0 {
        message.push_str(&format!("; already bound: {}", tally.already_bound));
    }
    if !tally.in_flight.is_empty() {
        // "in flight", not "populating": this list also carries `Pending` claims
        // (one parked on `AwaitingPodSchedule`, say), and reporting a claim that
        // has not started as "populating" sends the reader looking for a mover
        // Job that does not exist.
        message.push_str(&format!("; in flight: {}", tally.in_flight.join(", ")));
    }
    if !tally.failed.is_empty() {
        let failed: Vec<String> = tally
            .failed
            .iter()
            .map(|(name, reason)| format!("{name} ({reason})"))
            .collect();
        message.push_str(&format!("; failed: {}", failed.join(", ")));
        message.push_str(
            ". A failed claim stalls the Restore while its siblings continue; fix the cause and \
             re-create that claiming PVC to re-arm it.",
        );
    }
    let reason = match aggregate {
        ClaimsAggregate::NoClaims => AWAITING_PVC_DATA_SOURCE_REF_REASON,
        ClaimsAggregate::Failed(failed) => {
            debug_assert!(!failed.failed.is_empty());
            RESTORE_CLAIM_FAILED_REASON
        }
        ClaimsAggregate::InFlight(in_flight) => {
            debug_assert!(!in_flight.in_flight.is_empty());
            POPULATING_PRIME_PVC_REASON
        }
        ClaimsAggregate::AllSettled(settled) => {
            debug_assert!(settled.in_flight.is_empty());
            RESTORE_POPULATED_REASON
        }
    };
    (reason, message)
}

/// The prime PVC name for a claimant uid. ONE spelling, because the reaper has
/// to name the same object the driver created, and the legacy-adoption rule
/// reads it back off a LIST.
pub fn prime_pvc_name(consumer_uid: &str) -> String {
    format!("prime-{consumer_uid}")
}

/// Whether a restore mover `Job`'s NAME is re-used across runs.
///
/// Closed (and not a `bool`) because the two answers change how a TERMINATING
/// Job is read, and getting that backwards loses data either way: treat a
/// re-used name as fresh and a reaped-then-re-populated claim reads the outgoing
/// Job's `Succeeded` as its own, rebinding a still-empty prime; treat a fresh
/// name as re-used and a direct restore's TTL-reaped Job has its outcome dropped
/// and the whole restore re-runs over a target the workload may already be
/// writing to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobNameReuse {
    /// One run, one name — a direct restore (`{restore}`) or a per-claim
    /// populate (`{restore}-populate-{fnv8(uid)}`, #443). A terminating Job
    /// under this name IS this run's, and its mover writes
    /// `status.claims.<pvc>`.
    Fresh,
    /// The pre-#443 shared populate name `{restore}-populate`, reached only when
    /// driving a claim adopted from a legacy status. A terminating Job under it
    /// may belong to a PREVIOUS claim, so wait for the delete to land and create
    /// a fresh one — and its mover, which has no `claimKey`, pins the TOP-LEVEL
    /// `status.resolved`.
    LegacyShared,
}

/// The `status.resolved` a finalizing claim may read, in preference order.
/// Pure + exhaustive over [`JobNameReuse`].
///
/// A `Fresh` claim's mover writes `status.claims.<pvc>.resolved` and nothing
/// else, so it reads its own record and NEVER the top level — that is C1's
/// `resolved_at` no-fallback rule, and relaxing it would let one claim inherit a
/// sibling's (or a legacy) pin and report the wrong snapshot.
///
/// A `LegacyShared` claim is the one exception, and it is why this function
/// exists (#443 review item 5): the adopted `{restore}-populate` Job's work spec
/// carries no `claimKey`, so when its DEFERRED source resolves the mover pins
/// the TOP-LEVEL `status.resolved`. Adoption snapshots that field exactly once —
/// on the first post-upgrade pass, when it is typically still unpinned — so
/// without this fallback the finalize would read `None` and record
/// `NoSnapshotContinue` / "provisioned an empty volume" over a restore that
/// genuinely ran.
pub fn claim_finalize_pin(
    reuse: JobNameReuse,
    claim_pin: Option<kopiur_api::restore::ResolvedRestore>,
    prev_pin: Option<kopiur_api::restore::ResolvedRestore>,
    top_level: Option<kopiur_api::restore::ResolvedRestore>,
) -> Option<kopiur_api::restore::ResolvedRestore> {
    let own = claim_pin.or(prev_pin);
    match reuse {
        JobNameReuse::Fresh => own,
        JobNameReuse::LegacyShared => own.or(top_level),
    }
}

/// Whether a SETTLED claim's leftover populate artifacts may be swept on the
/// steady pass. Pure + exhaustive over the record's phase.
///
/// This is NARROWER than [`claim_artifacts_reapable`] on purpose, and the
/// difference is the #443 review's item 2. The steady sweep exists for ONE
/// shape: a `Populated`/`AlreadyBound` claim whose finalize crashed between
/// stripping the PV annotation and deleting the prime, leaving an orphan nothing
/// else will collect.
///
/// A **`Failed`** claim's artifacts are emphatically NOT that. Its record names
/// its mover `Job` and tells the user to read the Job/pod logs; restore movers
/// carry no TTL, so pre-#443 that Job (and the partially-written prime) survived
/// until the user acted, because `Failed` was terminal at the reconcile guard.
/// Sweeping them 120 s later would delete the evidence the message points at.
/// They are reclaimed when the claim is re-armed or its record is dropped — i.e.
/// when the user deletes and re-creates the claiming PVC, which is exactly the
/// documented remedy.
pub fn settled_artifacts_reapable(record: &RestoreClaimStatus) -> bool {
    match record.phase.as_ref() {
        Some(RestoreClaimPhase::Populated | RestoreClaimPhase::AlreadyBound) => {
            claim_artifacts_reapable(record.reason.as_deref())
        }
        Some(RestoreClaimPhase::Failed) => false,
        // Not reachable: `claim_drive` only answers `Settled` for a phase that
        // `RestoreClaimPhase::is_terminal` accepts, which is exactly the three
        // arms above. Encoded as "leave it alone" rather than a panic — a
        // hand-patched status must not be able to crash the reconcile loop, and
        // never touching an artifact is the safe answer to a state we did not
        // predict.
        None
        | Some(
            RestoreClaimPhase::Pending
            | RestoreClaimPhase::Populating
            | RestoreClaimPhase::Rebinding
            | RestoreClaimPhase::Unknown(_),
        ) => false,
    }
}

/// Whether `status` looks like it was written by a PRE-fan-out operator at all —
/// the gate on legacy adoption (#443 review item 8).
///
/// Without it, a brand-new `Restore`'s very first claim would "adopt" the
/// `Pending`/`AwaitingPvcDataSourceRef` park a zero-claim pass just wrote,
/// spending a `{restore}-populate` Job GET and logging that a pre-fan-out status
/// was adopted when none existed. The two real legacy signals are a pinned
/// top-level `status.resolved` (which only the pre-fan-out path ever wrote) and
/// a phase past resolution.
///
/// `status.target.pvcPrime` is deliberately NOT a signal: the zero-claim park
/// writes the sentinel `awaiting-claim` there on every new populator.
pub fn is_legacy_populator_status(status: &kopiur_api::RestoreStatus) -> bool {
    if !status.claims.is_empty() {
        return false;
    }
    if status.resolved.is_some() {
        return true;
    }
    // Exhaustive: a new phase has to decide whether reaching it implies the
    // pre-fan-out driver ran.
    match status.phase.as_ref() {
        Some(RestorePhase::Restoring | RestorePhase::Completed | RestorePhase::Failed) => true,
        None | Some(RestorePhase::Pending | RestorePhase::Resolving) => false,
        // A phase this build cannot read is not evidence of a LEGACY write — it
        // is evidence of a NEWER one, which would have written `claims`.
        Some(RestorePhase::Unknown(_)) => false,
    }
}

/// Which claimant a LEGACY (pre-fan-out) status belongs to — the one claim the
/// old single-claim driver was actually about. Pure.
///
/// Nothing records it: the pre-fan-out code took the first LIST hit. So it is
/// inferred, in order:
///
/// 1. **A surviving `prime-<uid>`.** The prime is uid-keyed, so this names the
///    claim mid-handshake exactly, bound or not.
/// 2. **Exactly one BOUND claimant.** Only a bound claim can have been
///    populated, and the old driver could only ever populate one — so with a
///    single bound claimant it is unambiguous however many claimants there are.
///    This is the #443 reporter's own post-upgrade state (N claimants, one
///    populated): adopting nothing there relabels the populated PVC
///    `TargetAlreadyBound` — "no restore ran, delete the PVC" — over the volume
///    holding the restored data.
///
/// A LONE claimant that is UNBOUND is deliberately NOT adopted. Pre-#443 that
/// shape (a `Completed` populator whose claim was deleted and re-applied) was
/// re-populated on the next pass; adopting `Populated` onto it would settle a
/// claim that never got its volume.
pub fn legacy_claimant<'a>(
    consumers: &'a [PersistentVolumeClaim],
    live_primes: &std::collections::BTreeSet<String>,
) -> Option<&'a PersistentVolumeClaim> {
    use kube::ResourceExt;
    if let Some(mid_handshake) = consumers.iter().find(|c| {
        c.uid()
            .is_some_and(|uid| live_primes.contains(&prime_pvc_name(&uid)))
    }) {
        return Some(mid_handshake);
    }
    let mut bound = consumers.iter().filter(|c| pvc_is_bound(c));
    match (bound.next(), bound.next()) {
        (Some(only_bound), None) => Some(only_bound),
        // Zero bound (nothing was ever populated) or several (the old driver
        // could not have populated more than one, so the extras were bound by
        // something else and we cannot tell which is ours): adopt nothing and
        // let every claimant drive fresh.
        (None, _) | (Some(_), Some(_)) => None,
    }
}

/// The status body for a populator `Restore` that NOTHING claims: the
/// Restore-level `AwaitingClaim=True` park, plus an explicit JSON `null` for
/// every claim record whose claimant is gone. Pure.
///
/// The nulls are the whole point (#443 review item 1). The pre-fix zero-claimant
/// branch wrote the park and dropped the reaper's `gone` list on the floor, so
/// the stale records survived — and the next pass, 30 s later, re-reaped them
/// and re-published `OrphanedPrimePvcReaped`, forever, for every app torn down
/// with its populator left standing. Writing the nulls in the SAME patch makes
/// the second pass a server-side no-op under
/// [`crate::io::status_merge_patch_is_noop`].
///
/// `target.pvcPrime: awaiting-claim` is the pre-#443 sentinel, kept verbatim so
/// the zero-claim surface consumers already watch does not change.
pub fn awaiting_claim_status(
    restore: &Restore,
    reason: &str,
    message: &str,
    gone: &[String],
) -> serde_json::Value {
    let conditions = io::upsert_condition(
        &existing_conditions(restore),
        "AwaitingClaim",
        true,
        reason,
        message,
        restore.metadata.generation,
    );
    let mut status =
        restore_ready_status_on(restore, &conditions, RestorePhase::Pending, reason, message);
    status["target"] = serde_json::json!({ "pvcPrime": "awaiting-claim" });
    if !gone.is_empty() {
        let mut nulls = serde_json::Map::new();
        for claim in gone {
            nulls.insert(claim.clone(), serde_json::Value::Null);
        }
        status["claims"] = serde_json::Value::Object(nulls);
    }
    status
}

/// Whether a fan-out pass has nothing observable left to do — the gate on
/// SKIPPING the cluster-wide `PersistentVolume` LIST, which is the expensive
/// read on this path. Pure.
///
/// Quiet means: no record to drop, every claim `Settled`, and no prime of any of
/// them still standing. A prime that will NEVER be reaped (a hijacked populate's,
/// kept on purpose — [`claim_artifacts_reapable`]) counts as absent: otherwise it
/// would buy one cluster-wide LIST every 600 s, forever, for a claim whose
/// artifacts nothing will ever collect.
///
/// `live` is `(claim name, claimant uid)` in the pass's drive order, paired with
/// `plans` positionally.
pub fn pass_is_all_quiet(
    gone: &[String],
    plans: &[ClaimDrive],
    live: &[(String, String)],
    prev: &std::collections::BTreeMap<String, RestoreClaimStatus>,
    live_primes: &std::collections::BTreeSet<String>,
) -> bool {
    gone.is_empty()
        && plans.iter().all(|d| *d == ClaimDrive::Settled)
        && live.iter().all(|(claim, uid)| {
            !live_primes.contains(&prime_pvc_name(uid))
                || !claim_artifacts_reapable(prev.get(claim).and_then(|r| r.reason.as_deref()))
        })
}

/// The end-of-pass status body for a fanned-out populator: the AGGREGATE phase,
/// the kstatus trio, `AwaitingClaim=False`, and every claim merge — plus an
/// explicit `null` for each record whose claimant is gone. Pure.
///
/// ONE body per pass, built in ONE place, because the fan-out has N claims and
/// only one conditions array: a per-claim `conditions` patch would replace that
/// array wholesale and erase the sibling written moments earlier (the
/// condition-writers-clobber class). Per-claim detail lives under `claims.<pvc>`
/// via [`claim_merge_body`], which never names a mover-owned key.
pub fn fanout_status(
    restore: &Restore,
    prev: &std::collections::BTreeMap<String, RestoreClaimStatus>,
    next: &std::collections::BTreeMap<String, RestoreClaimStatus>,
    gone: &[String],
) -> serde_json::Value {
    let aggregate = aggregate_claims(next);
    let (reason, message) = claims_summary(&aggregate);
    let conditions = io::upsert_condition(
        &existing_conditions(restore),
        "AwaitingClaim",
        false,
        crate::consts::CLAIMS_OBSERVED_REASON,
        "at least one PersistentVolumeClaim claims this Restore; status.claims carries the \
         per-claim state",
        restore.metadata.generation,
    );
    let mut status = restore_ready_status_on(
        restore,
        &conditions,
        aggregate_phase(&aggregate),
        reason,
        &message,
    );
    let mut merges = serde_json::Map::new();
    for (claim, record) in next {
        merges.insert(claim.clone(), claim_merge_body(prev.get(claim), record));
    }
    for claim in gone {
        merges.insert(claim.clone(), serde_json::Value::Null);
    }
    status["claims"] = serde_json::Value::Object(merges);
    status
}

/// What this pass must do with one claim record. Pure model, matched
/// exhaustively by the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimDrive {
    /// Run the handshake for this claim.
    Drive,
    /// The claim reached a terminal state under the LIVE claimant's uid — leave
    /// it alone.
    ///
    /// Keyed on the record's terminal phase, deliberately NOT on "bound and no
    /// prime": a `Failed` claim is unbound with its prime still standing, so the
    /// latter would re-drive it forever on the 120s cadence. Artifact reaping is
    /// a separate, reason-gated step (see [`claim_artifacts_reapable`]) — the
    /// prime of a hijacked populate holds half-written data and must survive.
    Settled,
    /// The record belongs to a DELETED claimant (its uid no longer matches the
    /// live PVC): reap the old claim's artifacts under `stale_uid`, then start
    /// over. Re-creating a claiming PVC is how a failed claim is re-armed.
    ReArm {
        /// The recorded (now dead) claimant uid, which names its prime PVC, Job
        /// and PV.
        stale_uid: String,
    },
}

/// Decide what to do with one claim record against the live claimant's uid.
/// Pure.
///
/// A record with no uid at all (a hand-patched or half-written entry) is never
/// RE-ARMED: there is nothing to reap under an unknown uid. It is then judged on
/// its phase like any other record — DRIVEN when unsettled (so the observed
/// handshake re-derives it), and [`ClaimDrive::Settled`] when its phase is
/// already terminal, which is the conservative direction: re-driving a record
/// that reads `Populated` would re-provision a prime over a claim that is done.
/// Only reachable via a hand-patched status — [`adopt_legacy_claim`] and the
/// driver both always seed the uid.
pub fn claim_drive(record: Option<&RestoreClaimStatus>, live_uid: &str) -> ClaimDrive {
    let Some(record) = record else {
        return ClaimDrive::Drive;
    };
    match record.uid.as_deref() {
        Some(uid) if uid != live_uid => ClaimDrive::ReArm {
            stale_uid: uid.to_string(),
        },
        Some(_) | None => match record.phase.as_ref() {
            Some(phase) if phase.is_terminal() => ClaimDrive::Settled,
            Some(_) | None => ClaimDrive::Drive,
        },
    }
}

/// The reason vocabulary a populator claim record carries.
///
/// An enum rather than string compares because one of these decisions is
/// destructive: [`Self::PopulateHijacked`] leaves its prime PVC standing ON
/// PURPOSE (it holds half-written data a human may want), and the reaper must
/// refuse it. A `reason == "PopulateHijacked"` test would silently stop matching
/// the day the literal moved; an exhaustive match over this enum cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimReason {
    /// No PVC claims the populator yet.
    AwaitingPvcDataSourceRef,
    /// A `WaitForFirstConsumer` claimant has no scheduling hint yet.
    AwaitingPodSchedule,
    /// The source snapshot has not appeared and the wait window is open.
    WaitingForSnapshot,
    /// The repository is not `Ready`.
    RepositoryNotReady,
    /// The referent the repository is derived from does not exist.
    RestoreReferentMissing,
    /// The source resolved to a concrete snapshot.
    SourceResolved,
    /// A mover is writing this claim's prime PVC.
    PopulatingPrimePvc,
    /// Deploy-or-restore: no snapshot matched and `Continue` chose empty.
    NoSnapshotContinue,
    /// This claim's populate landed and the claim is bound to it.
    RestoreSucceeded,
    /// The claim was already bound — a truthful no-op (#233).
    TargetAlreadyBound,
    /// A previously no-op'd populator's claim was re-created, so it re-resolves.
    ClaimRecreated,
    /// The mover `Job` failed.
    MoverJobFailed,
    /// The mover pod could not start within its deadline.
    MoverPodWedged,
    /// The claim was bound out from under a RUNNING populate.
    PopulateHijacked,
    /// `onMissingSnapshot: Fail` fired.
    SnapshotNotFound,
    /// The per-PVC kopia source path could not be derived (#443).
    SourcePathAmbiguous,
    /// Our rebind was issued but a different volume won the claim.
    LostRebind,
}

impl ClaimReason {
    /// Every reason, so the parse/label round-trip is a tripwire rather than a
    /// hand-maintained list.
    pub const ALL: &'static [Self] = &[
        Self::AwaitingPvcDataSourceRef,
        Self::AwaitingPodSchedule,
        Self::WaitingForSnapshot,
        Self::RepositoryNotReady,
        Self::RestoreReferentMissing,
        Self::SourceResolved,
        Self::PopulatingPrimePvc,
        Self::NoSnapshotContinue,
        Self::RestoreSucceeded,
        Self::TargetAlreadyBound,
        Self::ClaimRecreated,
        Self::MoverJobFailed,
        Self::MoverPodWedged,
        Self::PopulateHijacked,
        Self::SnapshotNotFound,
        Self::SourcePathAmbiguous,
        Self::LostRebind,
    ];

    /// The stored string for this reason. Exhaustive; every value comes from
    /// [`crate::consts`] so a reason written in one place and compared in another
    /// can never drift.
    pub fn as_str(self) -> &'static str {
        use crate::consts as c;
        match self {
            Self::AwaitingPvcDataSourceRef => c::AWAITING_PVC_DATA_SOURCE_REF_REASON,
            Self::AwaitingPodSchedule => c::AWAITING_POD_SCHEDULE_REASON,
            Self::WaitingForSnapshot => c::WAITING_FOR_SNAPSHOT_REASON,
            Self::RepositoryNotReady => c::RESTORE_REPOSITORY_NOT_READY_REASON,
            Self::RestoreReferentMissing => c::RESTORE_REFERENT_MISSING_REASON,
            Self::SourceResolved => c::RESTORE_SOURCE_RESOLVED_REASON,
            Self::PopulatingPrimePvc => c::POPULATING_PRIME_PVC_REASON,
            Self::NoSnapshotContinue => c::NO_SNAPSHOT_CONTINUE_REASON,
            Self::RestoreSucceeded => c::RESTORE_POPULATED_REASON,
            Self::TargetAlreadyBound => c::RESTORE_TARGET_ALREADY_BOUND_REASON,
            Self::ClaimRecreated => c::RESTORE_CLAIM_RECREATED_REASON,
            Self::MoverJobFailed => c::MOVER_JOB_FAILED_REASON,
            Self::MoverPodWedged => c::MOVER_POD_WEDGED_REASON,
            Self::PopulateHijacked => c::POPULATE_HIJACKED_REASON,
            Self::SnapshotNotFound => c::RESTORE_SNAPSHOT_NOT_FOUND_REASON,
            Self::SourcePathAmbiguous => c::SOURCE_PATH_AMBIGUOUS_REASON,
            Self::LostRebind => c::LOST_REBIND_REASON,
        }
    }

    /// Parse a stored reason string; `None` for anything outside the vocabulary
    /// (a hand-edited status, or a newer operator's reason).
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|r| r.as_str() == s)
    }

    /// Whether this claim's leftover populate artifacts (prime PVC, mover `Job`,
    /// work-spec `ConfigMap`, earmarked PV) may be reaped. Exhaustive.
    ///
    /// `PopulateHijacked` is the one `false`. `fail_populate_hijacked` leaves the
    /// prime PVC standing because it holds HALF-WRITTEN data the admin may want
    /// to inspect, and what protects it today is the whole populator
    /// short-circuiting on `Failed` at [`phase_is_terminal_at_guard`]. The fan-out
    /// has to relax that guard (a failed claim must not stop its siblings), so
    /// this predicate is where the protection lands instead — and the two must
    /// change together: C2 (#443) relaxes the guard in the same commit that gates
    /// `reap_claim_artifacts` on this.
    pub fn artifacts_reapable(self) -> bool {
        match self {
            Self::PopulateHijacked => false,
            Self::AwaitingPvcDataSourceRef
            | Self::AwaitingPodSchedule
            | Self::WaitingForSnapshot
            | Self::RepositoryNotReady
            | Self::RestoreReferentMissing
            | Self::SourceResolved
            | Self::PopulatingPrimePvc
            | Self::NoSnapshotContinue
            | Self::RestoreSucceeded
            | Self::TargetAlreadyBound
            | Self::ClaimRecreated
            | Self::MoverJobFailed
            | Self::MoverPodWedged
            | Self::SnapshotNotFound
            | Self::SourcePathAmbiguous
            | Self::LostRebind => true,
        }
    }
}

/// Whether a claim recorded with `reason` may have its populate artifacts
/// reaped. An unrecognized reason is reapable — that matches the pre-#443
/// behavior, where `PopulateHijacked` was the only special case. Pure.
pub fn claim_artifacts_reapable(reason: Option<&str>) -> bool {
    match reason.and_then(ClaimReason::parse) {
        Some(r) => r.artifacts_reapable(),
        None => true,
    }
}

/// Seed a claim record from a LEGACY single-claim `Restore` status — the upgrade
/// path (#443).
///
/// Fires only when `status.claims` is empty and the object already carries a
/// phase, i.e. a `Restore` written by a pre-fan-out operator. Existing
/// single-PVC populators then keep working with zero spec change and no status
/// regression: an in-flight populate is adopted and driven (under its legacy Job
/// name, via `legacy_job`, so a second mover is never launched into the same
/// prime), and a finished one is recorded as finished rather than re-run.
///
/// The phase is keyed on the legacy TOP-LEVEL phase first and the Ready reason
/// only to disambiguate `Completed`, which is three different situations:
/// a genuine success, the #233 already-bound no-op, and the legacy stuck orphan
/// (`Completed` + `PopulatingPrimePvc`) that still needs driving. A
/// `RestoreSucceeded` legacy is emphatically NOT relabeled `AlreadyBound` — that
/// would tell the user no restore ever ran.
pub fn adopt_legacy_claim(
    status: &kopiur_api::RestoreStatus,
    consumer: &PersistentVolumeClaim,
    legacy_job: Option<&str>,
) -> Option<RestoreClaimStatus> {
    if !status.claims.is_empty() {
        return None;
    }
    let phase = status.phase.as_ref()?;
    let ready = status
        .conditions
        .iter()
        .find(|c| c.type_ == crate::consts::READY_CONDITION);
    let reason = ready.map(|c| c.reason.as_str());
    Some(RestoreClaimStatus {
        uid: consumer.metadata.uid.clone(),
        phase: Some(legacy_claim_phase(phase, reason)),
        reason: reason.map(str::to_string),
        message: ready.map(|c| c.message.clone()),
        source_path: status
            .resolved
            .as_ref()
            .and_then(|r| r.identity.as_ref())
            .and_then(|i| i.source_path.clone()),
        resolved: status.resolved.clone(),
        pvc_prime: status.target.as_ref().and_then(|t| t.pvc_prime.clone()),
        job: legacy_job.map(str::to_string),
        wait_started_at: status.wait_started_at.clone(),
        // Mover-owned; a controller-side adoption never claims to have written
        // them, so `claim_merge_body` has nothing of the mover's to clobber.
        observed_at: None,
        log_tail: None,
        failure: None,
    })
}

/// The claim phase a legacy `(top-level phase, Ready reason)` pair maps to.
/// Pure + exhaustive over [`RestorePhase`].
fn legacy_claim_phase(phase: &RestorePhase, reason: Option<&str>) -> RestoreClaimPhase {
    match phase {
        // `Completed` is three situations, told apart by the Ready reason.
        // Exhaustive over `Option<ClaimReason>` — no `_ =>` — so a reason added
        // later must decide what a legacy `Completed` carrying it adopted as.
        RestorePhase::Completed => match reason.and_then(ClaimReason::parse) {
            Some(ClaimReason::TargetAlreadyBound) => RestoreClaimPhase::AlreadyBound,
            Some(ClaimReason::RestoreSucceeded | ClaimReason::NoSnapshotContinue) => {
                RestoreClaimPhase::Populated
            }
            // The legacy stuck orphan (`Completed` + `PopulatingPrimePvc`) and
            // every other reason, including one this build cannot place (`None`):
            // drive it and let the observed handshake settle it. Re-driving a
            // populator is idempotent (its source is pinned), so this is the safe
            // default — claiming `Populated` over a rebind that never happened is
            // not.
            None
            | Some(
                ClaimReason::AwaitingPvcDataSourceRef
                | ClaimReason::AwaitingPodSchedule
                | ClaimReason::WaitingForSnapshot
                | ClaimReason::RepositoryNotReady
                | ClaimReason::RestoreReferentMissing
                | ClaimReason::SourceResolved
                | ClaimReason::PopulatingPrimePvc
                | ClaimReason::ClaimRecreated
                | ClaimReason::MoverJobFailed
                | ClaimReason::MoverPodWedged
                | ClaimReason::PopulateHijacked
                | ClaimReason::SnapshotNotFound
                | ClaimReason::SourcePathAmbiguous
                | ClaimReason::LostRebind,
            ) => RestoreClaimPhase::Populating,
        },
        RestorePhase::Failed => RestoreClaimPhase::Failed,
        RestorePhase::Restoring => RestoreClaimPhase::Populating,
        RestorePhase::Pending | RestorePhase::Resolving => RestoreClaimPhase::Pending,
        // An unreadable phase is driven, not settled.
        RestorePhase::Unknown(_) => RestoreClaimPhase::Pending,
    }
}

/// [`super::pinned_decision`] for ONE claim record (#443) — all four of its arms,
/// keyed on the claim's own pin and phase rather than the Restore's.
///
/// - `resolution: NoSnapshot` pinned → [`super::Resolution::Empty`].
/// - `kopiaSnapshotID` pinned (with or without the newer `resolution`, so a pin
///   written before that field existed still reads) → `Snapshot`.
/// - Unpinned but the claim already reads `Populated`: the pre-fix `Continue`
///   arm stamped completion without pinning anything, and a snapshot-resolved
///   claim always pins before it can be `Populated` — so the decision WAS
///   "empty". Back-fill it rather than re-resolve, or a snapshot that appeared
///   after the original decision would silently restore over the volume.
/// - Unpinned and `AlreadyBound` is the #233 carve-out: that claim never ran a
///   mover, and the mover is what pins a deferred source. Back-filling
///   `NoSnapshot` there would durably record "this restore decided to come up
///   empty", so a later, legitimate re-creation of the claiming PVC would
///   provision an EMPTY volume instead of restoring.
///
/// Anything else → `None` (resolve now). Pure + exhaustive.
pub fn claim_pinned_decision(record: &RestoreClaimStatus) -> Option<super::Resolution> {
    use kopiur_api::ResolutionOutcome;
    match &record.resolved {
        Some(r) if r.resolution == Some(ResolutionOutcome::NoSnapshot) => {
            Some(super::Resolution::Empty)
        }
        Some(r) => r.kopia_snapshot_id.clone().map(super::Resolution::Snapshot),
        None => match record.phase.as_ref() {
            Some(RestoreClaimPhase::Populated) => Some(super::Resolution::Empty),
            None
            | Some(
                RestoreClaimPhase::Pending
                | RestoreClaimPhase::Populating
                | RestoreClaimPhase::Rebinding
                | RestoreClaimPhase::AlreadyBound
                | RestoreClaimPhase::Failed
                | RestoreClaimPhase::Unknown(_),
            ) => None,
        },
    }
}

/// The controller-owned keys of a claim record, in wire spelling. Anything not
/// listed here is the mover's and the controller never writes (or nulls) it.
const CLAIM_CONTROLLER_KEYS: &[&str] = &[
    "uid",
    "phase",
    "reason",
    "message",
    "sourcePath",
    "pvcPrime",
    "job",
    "waitStartedAt",
];

/// The mover-owned keys of a claim record. The controller strips these from
/// every merge body it builds, so a controller pass can never blank a value the
/// claim's own mover wrote.
const CLAIM_MOVER_KEYS: &[&str] = &["observedAt", "logTail", "failure"];

/// The merge-patch value for one claim's entry: `next`, minus the mover-owned
/// keys, plus an explicit JSON `null` for every controller-owned key that `prev`
/// had and `next` does not.
///
/// The nulls are the whole point and cannot be expressed by the typed status: an
/// RFC-7386 merge patch removes only the keys it names, and every field is
/// `skip_serializing_if = "Option::is_none"`, so a `None` serializes to nothing
/// and would leave (say) a spent `waitStartedAt` or a reaped `pvcPrime` standing
/// forever.
///
/// `resolved` is written when `next` has one (the controller pinned it) and
/// never nulled — a claim's pin is durable, and the mover writes it too.
pub fn claim_merge_body(
    prev: Option<&RestoreClaimStatus>,
    next: &RestoreClaimStatus,
) -> serde_json::Value {
    // `expect`, not a silent `unwrap_or_else(json!({}))`: degrading to an empty
    // patch here would drop a claim's whole state update with nothing in the log.
    // `RestoreClaimStatus` is plain `Option`s over strings, unit enums and derived
    // structs — no custom `Serialize`, no non-string map key — so `to_value` is
    // infallible and always yields an object.
    let claim_object = |record: &RestoreClaimStatus| match serde_json::to_value(record)
        .expect("RestoreClaimStatus serializes: plain Options, no custom Serialize")
    {
        serde_json::Value::Object(map) => map,
        other => unreachable!("a struct serializes to a JSON object, got {other}"),
    };

    let mut obj = claim_object(next);
    for key in CLAIM_MOVER_KEYS {
        obj.remove(*key);
    }
    if let Some(prev) = prev {
        let prev_obj = claim_object(prev);
        for key in CLAIM_CONTROLLER_KEYS {
            if prev_obj.contains_key(*key) && !obj.contains_key(*key) {
                obj.insert((*key).to_string(), serde_json::Value::Null);
            }
        }
    }
    serde_json::Value::Object(obj)
}

/// Whether a claim record CHANGED in a way worth an Event — the `(phase, reason)`
/// pair moved, or the record is new. Pure.
///
/// This is what stops Event `count` inflation on the 120s heartbeat: the recorder
/// aggregates identical Events into one object, so a per-pass emit would spin a
/// single Event's count rather than record anything new. Phases are compared by
/// their labels, which is a total comparison over the enum (a new variant cannot
/// silently land on the "unchanged" side).
pub fn claim_transition(prev: Option<&RestoreClaimStatus>, next: &RestoreClaimStatus) -> bool {
    use kopiur_api::common::PhaseLabel;
    let label = |r: &RestoreClaimStatus| r.phase.as_ref().map(|p| p.label().to_string());
    match prev {
        None => true,
        Some(prev) => label(prev) != label(next) || prev.reason != next.reason,
    }
}

/// The `waitTimeout` window for ONE claim (#443).
///
/// Per-claim because claimants appear at different times: a sibling created an
/// hour after the first must get its own full window, not the remains of the
/// first one's. Anchored at the claim record's `waitStartedAt` once stamped;
/// failing that, at the Restore's legacy top-level `waitStartedAt` (so an upgrade
/// mid-wait does not silently restart the window); failing that, at `now` —
/// this pass is the first on which the claim could proceed, and the caller stamps
/// it into the record.
///
/// Always [`WaitWindow::Open`]: a claim record exists only because a PVC claims
/// the populator, which is exactly the condition
/// [`WaitWindow::AwaitingClaim`] describes the absence of. That state stays the
/// Restore-level one, for zero claims. Pure.
pub fn claim_wait_window(
    record: Option<&RestoreClaimStatus>,
    restore: &Restore,
    now_epoch: i64,
) -> WaitWindow {
    let parse = |at: &str| {
        chrono::DateTime::parse_from_rfc3339(at)
            .ok()
            .map(|t| t.timestamp())
    };
    let anchor = record
        .and_then(|r| r.wait_started_at.as_deref())
        .and_then(parse)
        .or_else(|| {
            restore
                .status
                .as_ref()
                .and_then(|s| s.wait_started_at.as_deref())
                .and_then(parse)
        })
        .unwrap_or(now_epoch);
    WaitWindow::Open(anchor)
}
