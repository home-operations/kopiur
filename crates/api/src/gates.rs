//! The **structural-gate registry**: the one enumeration of the condition
//! `type`/`status`/`reason` triples that mean "this object is blocked on
//! something only a human can change".
//!
//! A *structural* gate is not a transient retry. It never self-heals: the
//! reconciler parks the object (usually at `phase: Pending`) and waits for an
//! out-of-band change — a namespace opt-in annotation, a credentials `Secret`,
//! an acknowledgement timestamp. Because the phase itself stays unremarkable,
//! anything that diagnoses a cluster by phase alone reports all-green while the
//! work is wedged (issue #359: `kubectl kopiur doctor` passed all checks with a
//! `Snapshot` stuck on `MoverPermitted=False`).
//!
//! The fix is to make the gate set **shared by construction**. The controller
//! writes conditions from these rows and the CLI's `doctor` iterates the same
//! rows, so a gate added on the server side cannot be invisible to the client
//! side: there is exactly one list, in `kopiur-api`, which both depend on.
//!
//! This module is pure data + pure functions — no `kube::Client`, no `tokio` —
//! per the `api` ↔ `controller` split.

use crate::consts;

/// Which CR kinds a structural gate's condition is written on.
///
/// Deliberately coarse (kinds, not selectors): a gate row answers "when I look
/// at an object of THIS kind, is this condition meaningful?", which is all a
/// diagnostic needs to avoid hunting for a condition that can never appear.
///
/// ```
/// use kopiur_api::gates::GateScope;
///
/// // A privileged-mover refusal is written on both work kinds.
/// assert!(GateScope::SnapshotOrRestore.covers_snapshot());
/// assert!(GateScope::SnapshotOrRestore.covers_restore());
/// // The mass-deletion breaker's per-Snapshot hold is Snapshot-only.
/// assert!(GateScope::Snapshot.covers_snapshot());
/// assert!(!GateScope::Snapshot.covers_restore());
/// // Repository gates live on Repository/ClusterRepository.
/// assert!(GateScope::Repository.covers_repository());
/// assert!(!GateScope::Repository.covers_snapshot());
/// // A schedule-level block lives on the SnapshotSchedule itself.
/// assert!(GateScope::SnapshotSchedule.covers_snapshot_schedule());
/// assert!(!GateScope::Snapshot.covers_snapshot_schedule());
/// // A recipe-level block lives on the SnapshotPolicy itself.
/// assert!(GateScope::SnapshotPolicy.covers_snapshot_policy());
/// assert!(!GateScope::SnapshotPolicy.covers_snapshot());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GateScope {
    /// Written on both `Snapshot` and `Restore` CRs (the two "work" kinds that
    /// launch mover Jobs, and whose reconcilers share the gate).
    SnapshotOrRestore,
    /// Written on `Snapshot` CRs only.
    Snapshot,
    /// Written on `Repository` and `ClusterRepository` CRs.
    Repository,
    /// Written on `SnapshotSchedule` CRs only — the schedule itself is blocked,
    /// as opposed to any individual run being blocked.
    SnapshotSchedule,
    /// Written on `SnapshotPolicy` CRs only — the recipe itself is (partially)
    /// blocked, as opposed to any individual run being blocked.
    SnapshotPolicy,
}

impl GateScope {
    /// Whether a `Snapshot` can carry a gate of this scope. Exhaustive.
    pub fn covers_snapshot(self) -> bool {
        match self {
            Self::SnapshotOrRestore | Self::Snapshot => true,
            Self::Repository | Self::SnapshotSchedule | Self::SnapshotPolicy => false,
        }
    }

    /// Whether a `Restore` can carry a gate of this scope. Exhaustive.
    pub fn covers_restore(self) -> bool {
        match self {
            Self::SnapshotOrRestore => true,
            Self::Snapshot | Self::Repository | Self::SnapshotSchedule | Self::SnapshotPolicy => {
                false
            }
        }
    }

    /// Whether a `Repository`/`ClusterRepository` can carry a gate of this
    /// scope. Exhaustive.
    pub fn covers_repository(self) -> bool {
        match self {
            Self::Repository => true,
            Self::SnapshotOrRestore
            | Self::Snapshot
            | Self::SnapshotSchedule
            | Self::SnapshotPolicy => false,
        }
    }

    /// Whether a `SnapshotSchedule` can carry a gate of this scope. Exhaustive.
    pub fn covers_snapshot_schedule(self) -> bool {
        match self {
            Self::SnapshotSchedule => true,
            Self::SnapshotOrRestore | Self::Snapshot | Self::Repository | Self::SnapshotPolicy => {
                false
            }
        }
    }

    /// Whether a `SnapshotPolicy` can carry a gate of this scope. Exhaustive.
    pub fn covers_snapshot_policy(self) -> bool {
        match self {
            Self::SnapshotPolicy => true,
            Self::SnapshotOrRestore
            | Self::Snapshot
            | Self::Repository
            | Self::SnapshotSchedule => false,
        }
    }
}

/// How loudly a tripped gate should be reported.
///
/// `Fail` means "this will never complete until a human acts"; `Warn` means
/// "this is blocked, but the block may well be the configuration you asked
/// for" — so it must not, on its own, turn a diagnostic red.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateSeverity {
    /// Work is wedged and cannot progress without an out-of-band change.
    Fail,
    /// Work is refused, but the refusal is a plausible deliberate choice.
    Warn,
}

impl GateSeverity {
    /// Stable display string (exhaustive `match`), for CLI output and tests.
    pub fn label(self) -> &'static str {
        match self {
            Self::Fail => "Fail",
            Self::Warn => "Warn",
        }
    }
}

/// The Kubernetes condition `status` string `"True"`. Named so a gate row's
/// polarity is spelled out rather than being a bare literal at the row.
pub const CONDITION_TRUE: &str = "True";
/// The Kubernetes condition `status` string `"False"`.
pub const CONDITION_FALSE: &str = "False";

/// One human-actionable structural gate: a condition `type`, the `status` that
/// means BLOCKED, the `reason` the writer stamps, the CR kinds it appears on,
/// and how loudly to report it.
///
/// Polarity is per-row rather than implied, because kopiur has gates of both
/// shapes: `MoverPermitted=False` blocks, and so does `DeletionHeld=True`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StructuralGate {
    /// The CR kinds this gate's condition is written on.
    pub applies_to: GateScope,
    /// The condition `type` (a `consts` string, never a literal at the call site).
    pub condition: &'static str,
    /// The condition `status` that means BLOCKED — [`CONDITION_FALSE`] or
    /// [`CONDITION_TRUE`].
    pub blocked_status: &'static str,
    /// The `reason` the reconciler stamps when it writes the blocked condition.
    pub reason: &'static str,
    /// How loudly a tripped gate is reported.
    pub severity: GateSeverity,
}

impl StructuralGate {
    /// Whether a live condition **is** this gate: `type`, `status`, AND `reason`
    /// all match.
    ///
    /// This is the matcher consumers should reach for. `condition` + `status`
    /// alone do not identify a row — `CredentialsAvailable=False` is written
    /// with several different reasons and therefore has several rows — so
    /// filtering the registry on [`trips`](Self::trips) alone yields multiple
    /// hits for one live condition and double-reports it. `matches` selects exactly one row per
    /// live condition, which is the property
    /// `gate_rows_are_unique_and_internally_consistent` pins.
    ///
    /// ```
    /// use kopiur_api::gates::STRUCTURAL_GATES;
    ///
    /// // One live condition off a wedged Snapshot ⇒ exactly one registry row.
    /// let hits: Vec<_> = STRUCTURAL_GATES
    ///     .iter()
    ///     .filter(|g| g.matches("CredentialsAvailable", "False", "MissingCredentialsSecret"))
    ///     .collect();
    /// assert_eq!(hits.len(), 1);
    /// assert_eq!(hits[0].reason, "MissingCredentialsSecret");
    /// // The other reason selects the other row, never both.
    /// assert!(!hits[0].matches("CredentialsAvailable", "False", "MissingServiceAccount"));
    /// ```
    pub fn matches(&self, condition_type: &str, status: &str, reason: &str) -> bool {
        self.trips(condition_type, status) && reason == self.reason
    }

    /// The **reason-agnostic** coarse filter: whether a live condition's
    /// `type`/`status` pair is the blocked polarity of this gate's condition.
    ///
    /// The polarity comparison lives here so no consumer re-derives it (the
    /// `!= "True"` / `== "True"` mix-ups this registry exists to prevent). Use
    /// it to answer "is this condition one of the ones we gate on at all?" —
    /// notably when a live condition carries a reason NO row covers (a newer
    /// operator's reason string), where [`matches`](Self::matches) would
    /// silently report nothing. To identify WHICH row a condition is, use
    /// `matches`: `trips` can match several rows sharing a condition+polarity.
    ///
    /// ```
    /// use kopiur_api::gates::{STRUCTURAL_GATES, GateScope};
    ///
    /// let mover = STRUCTURAL_GATES
    ///     .iter()
    ///     .find(|g| g.condition == "MoverPermitted")
    ///     .expect("the privileged-mover gate is registered");
    /// assert!(mover.trips("MoverPermitted", "False"));
    /// assert!(!mover.trips("MoverPermitted", "True"));
    /// assert!(!mover.trips("Ready", "False"));
    /// assert_eq!(mover.applies_to, GateScope::SnapshotOrRestore);
    ///
    /// // Coarse by design: an unregistered reason still flags the condition.
    /// assert!(mover.trips("MoverPermitted", "False"));
    /// ```
    pub fn trips(&self, condition_type: &str, status: &str) -> bool {
        condition_type == self.condition && status == self.blocked_status
    }

    /// [`blocked_status`](Self::blocked_status) as the `bool` a condition writer
    /// passes when it writes this gate as BLOCKED — `true` for
    /// [`CONDITION_TRUE`], `false` for [`CONDITION_FALSE`].
    ///
    /// The controller's `upsert_condition` takes a `bool`, so without this every
    /// registry-driven writer would hand-translate the status string at its own
    /// call site — exactly the per-site re-derivation this registry exists to
    /// remove. Any other string reads as `false`; the
    /// `every_gate_row_is_well_formed` tripwire makes that unreachable.
    ///
    /// ```
    /// use kopiur_api::gates::STRUCTURAL_GATES;
    ///
    /// let mover = STRUCTURAL_GATES
    ///     .iter()
    ///     .find(|g| g.condition == "MoverPermitted")
    ///     .expect("registered");
    /// assert!(!mover.blocked_is_true()); // MoverPermitted=False blocks
    ///
    /// let held = STRUCTURAL_GATES
    ///     .iter()
    ///     .find(|g| g.condition == "DeletionHeld")
    ///     .expect("registered");
    /// assert!(held.blocked_is_true()); // DeletionHeld=True blocks
    /// ```
    pub fn blocked_is_true(&self) -> bool {
        self.blocked_status == CONDITION_TRUE
    }
}

/// An elevated mover in a namespace that has not opted in. The admin adds the
/// `privileged-movers` annotation out-of-band; until then the object sits at
/// `phase: Pending` — the exact shape of #359.
pub const PRIVILEGED_MOVER_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::SnapshotOrRestore,
    condition: consts::MOVER_PERMITTED_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::PRIVILEGED_MOVER_NOT_PERMITTED_REASON,
    severity: GateSeverity::Fail,
};

/// The mover's credential `Secret` is not in the workload namespace. Parks at
/// `phase: Pending` until the user creates it (or enables projection).
pub const MISSING_CREDENTIALS_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::SnapshotOrRestore,
    condition: consts::CREDENTIALS_AVAILABLE_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::MISSING_CREDENTIALS_REASON,
    severity: GateSeverity::Fail,
};

/// Same condition as [`MISSING_CREDENTIALS_GATE`], different missing
/// dependency: the workload-identity `ServiceAccount` the backend names.
/// kopiur never creates it.
pub const MISSING_SERVICE_ACCOUNT_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::SnapshotOrRestore,
    condition: consts::CREDENTIALS_AVAILABLE_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::MISSING_SERVICE_ACCOUNT_REASON,
    severity: GateSeverity::Fail,
};

/// Same condition as [`MISSING_CREDENTIALS_GATE`], a third missing
/// dependency: the backend's `tls.caBundleRef` ConfigMap (the PEM CA bundle
/// for a private-CA S3 endpoint). kopiur never creates it, and a ConfigMap
/// that never appears never self-heals — the exact phase-invisible park shape
/// of #359.
pub const MISSING_CA_BUNDLE_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::SnapshotOrRestore,
    condition: consts::CREDENTIALS_AVAILABLE_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::MISSING_CA_BUNDLE_REASON,
    severity: GateSeverity::Fail,
};

/// Inverted polarity: the per-`Snapshot` hold the mass-deletion breaker
/// applies. Released only by the `allow-mass-deletion` acknowledgement on the
/// repository, so it is squarely "needs a human".
pub const DELETION_HELD_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::Snapshot,
    condition: consts::DELETION_HELD_CONDITION,
    blocked_status: CONDITION_TRUE,
    reason: consts::MASS_DELETION_BREAKER_REASON,
    severity: GateSeverity::Fail,
};

/// The repository-level view of the same breaker: a whole wave is held.
pub const MASS_DELETION_HELD_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::Repository,
    condition: consts::MASS_DELETION_HELD_CONDITION,
    blocked_status: CONDITION_TRUE,
    reason: consts::MASS_DELETION_THRESHOLD_EXCEEDED_REASON,
    severity: GateSeverity::Fail,
};

/// A backup refused because its repository is `mode: ReadOnly`.
///
/// Unlike the other rows this is NOT an invisible park: the writer
/// (`snapshot/mod.rs`) sets `phase: Failed` in the same status patch, so a
/// phase-based check already sees the object. It is registered for EXPLANATORY
/// value — it turns "this backup failed" into "this backup was refused because
/// the repository is read-only" without a second lookup. Hence WARN, not Fail:
/// a read-only repository is a legitimate, deliberate configuration (a
/// replication target, an archived repo served for restores only), so a
/// green/red verdict must not hinge on it.
pub const REPOSITORY_READ_ONLY_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::Snapshot,
    condition: consts::REPOSITORY_WRITABLE_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::REPOSITORY_READ_ONLY_REASON,
    severity: GateSeverity::Warn,
};

/// Version skew, one kind removed: a previous run of this schedule sits at a
/// phase string this build cannot interpret, so it can never be observed to
/// finish. Under the default `concurrencyPolicy: Forbid` the schedule stops
/// firing FOREVER while looking perfectly healthy — the concurrency gate is
/// doing exactly what it was asked to. The out-of-band change that clears it is
/// finishing the operator rollout (or deleting the wedged `Snapshot`), which is
/// squarely "needs a human", so: Fail.
pub const SCHEDULE_BLOCKED_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::SnapshotSchedule,
    condition: consts::SCHEDULE_RUNNABLE_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::BLOCKED_ON_UNREADABLE_RUN_REASON,
    severity: GateSeverity::Fail,
};

/// A `SnapshotPolicy` referencing at least one repository that is not `Ready`.
/// The policy reconciler keeps processing the READY subset (multi-repository
/// fan-out, #368) but defers backups/retention/adoption/verification against
/// the rest, and requeues until every repository recovers. Warn, not Fail: a
/// repository deliberately taken down (a migration, a powered-off NAS target)
/// is a plausible operator choice, the ready subset keeps working, and the
/// per-child parks behind the same outage already carry their own reporting
/// (`kopiur_snapshot_gated`, `KopiurRepositoryNotReady`) — so this gate must
/// explain, not independently turn a diagnostic red.
pub const POLICY_REPOSITORY_NOT_READY_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::SnapshotPolicy,
    condition: consts::REPOSITORIES_READY_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::REPOSITORY_NOT_READY_REASON,
    severity: GateSeverity::Warn,
};

/// A `SnapshotSchedule` whose members × repositories cross-product exceeds the
/// fan-out cap: the fired slot was SKIPPED, and every future slot will keep
/// skipping until the selector is narrowed or the policy's repository list
/// shrunk — no backups run while everything else looks healthy, the exact
/// silent-wedge shape of #359. The writer asserts BOTH polarities each fire
/// pass, so the gate self-clears the moment a slot mints fully. Promoted into
/// the registry by the #368 M10 gates/doctor checklist (previously a
/// controller-internal condition doctor could not see).
pub const SCHEDULE_FANOUT_CAPPED_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::SnapshotSchedule,
    condition: consts::SCHEDULE_FANOUT_CAPPED_CONDITION,
    blocked_status: CONDITION_TRUE,
    reason: consts::FANOUT_TOO_LARGE_REASON,
    severity: GateSeverity::Fail,
};

/// A backup whose DIRECT source PVC (`spec.sources[].pvc`) does not exist at
/// launch time. The `Snapshot` parks at `phase: Pending` on the slow
/// structural cadence and, once the controller's missing-source deadline
/// passes, flips terminally `Failed` — a PVC that never reappears never
/// self-heals, and only recreating it (or repointing the policy's sources) can
/// clear the block. Written on the Snapshot only for the direct-source case: a
/// vanished operator-staged claim (copyMethod Snapshot/Clone) is a restage
/// race and stays a plain transient retry, never this gate.
pub const SOURCE_PVC_MISSING_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::Snapshot,
    condition: consts::SOURCE_PVC_AVAILABLE_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::SOURCE_PVC_MISSING_REASON,
    severity: GateSeverity::Fail,
};

/// A `spec.seed` in migrate mode whose SOURCE repository is missing or not
/// `Ready` (issue #380). The repository parks `Pending` with `Seeded=False`
/// and re-checks; nothing in kopiur can bring the source up, so it is squarely
/// "needs a human" — and the park is phase-INVISIBLE in the worst way: a
/// brand-new repository sitting at `Pending` looks identical to one that is
/// simply still bootstrapping, and it will sit there for as long as the source
/// is down.
///
/// WARN rather than Fail, for two reasons that point the same way. The
/// registry pins one severity per condition+scope
/// (`gate_rows_are_unique_and_internally_consistent`), and the sibling
/// [`SEEDING_GATE`] on this same condition *must* be Warn — a seed legitimately
/// runs for hours, and a healthy copy must not turn a diagnostic red. And on
/// its own merits this park is the same shape as
/// [`POLICY_REPOSITORY_NOT_READY_GATE`]: a source repository that is still
/// bootstrapping alongside this one (the ordinary DR bring-up) comes up by
/// itself, and one deliberately taken down is a plausible operator choice. The
/// block is reported either way, with the writer's message — which is what
/// makes it worth registering.
pub const SEED_SOURCE_NOT_READY_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::Repository,
    condition: consts::SEEDED_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::WAITING_FOR_SEED_SOURCE_REASON,
    severity: GateSeverity::Warn,
};

/// A migrate-mode `spec.seed` whose LOCAL backend and resolved SOURCE
/// repository disagree on workload identity (issue #380).
///
/// The blob arm of this rule is refused at admission
/// (`validate_replication_auth`), but a `seed.from.repository` reference hides
/// the source's backend from a spec-only validator, so the controller
/// re-applies the same rule once it has resolved the source and parks here
/// instead. Without the park the CR is admitted, a Job is launched, and the
/// failure surfaces as a bare cloud auth error from whichever side the pod's
/// single ServiceAccount is not.
///
/// WARN for the same reason every other `Seeded` row is: the registry pins one
/// severity per condition+scope, and the in-flight [`SEEDING_GATE`] must not
/// turn a diagnostic red. Nothing is lost — the message names both
/// ServiceAccounts and the two ways out, and the repository is not `Ready`,
/// which a diagnostic fails on independently.
pub const SEED_SOURCE_AUTH_CONFLICT_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::Repository,
    condition: consts::SEEDED_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::SEED_SOURCE_AUTH_CONFLICT_REASON,
    severity: GateSeverity::Warn,
};

/// A seeding bootstrap Job is in flight (issue #380): the repository is
/// copying a whole repository across and is legitimately not `Ready` yet.
///
/// Registered for EXPLANATORY value, like [`REPOSITORY_READ_ONLY_GATE`]: a seed
/// runs for hours by design (its Job deadline defaults to 24h), so a
/// diagnostic must be able to say "seeding" rather than "stuck at Pending".
/// Hence WARN — progress is not a fault, and this row must never on its own
/// turn a diagnostic red.
pub const SEEDING_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::Repository,
    condition: consts::SEEDED_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::SEEDING_REASON,
    severity: GateSeverity::Warn,
};

/// A `spec.seed` whose SOURCE answered but holds no kopia repository. Retried
/// automatically every ~2 minutes, so it clears itself the moment the source
/// exists — but until then the repository never becomes `Ready`, and the fix
/// (repoint `spec.seed.from`) is squarely out-of-band.
pub const SEED_SOURCE_NOT_FOUND_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::Repository,
    condition: consts::SEEDED_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::SEED_SOURCE_NOT_FOUND_REASON,
    severity: GateSeverity::Warn,
};

/// A `spec.seed` whose source is a real repository holding zero snapshots, with
/// `allowEmptySource` at its `false` default. Blocking `Ready` is the point: a
/// valid-but-empty mirror is nearly always mis-pointed.
pub const SEED_SOURCE_EMPTY_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::Repository,
    condition: consts::SEEDED_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::SEED_SOURCE_EMPTY_REASON,
    severity: GateSeverity::Warn,
};

/// A migrate-mode seed whose post-verify found snapshots missing. The next
/// attempt resumes the copy, so this converges on its own — but a seed that
/// keeps being cut short needs a human to raise its deadline.
pub const SEED_INCOMPLETE_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::Repository,
    condition: consts::SEEDED_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::SEED_INCOMPLETE_REASON,
    severity: GateSeverity::Warn,
};

/// A seed that was armed and left the repository holding ZERO snapshots. Like
/// [`SEED_INCOMPLETE_GATE`], the next attempt resumes the copy.
pub const SEED_LEFT_EMPTY_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::Repository,
    condition: consts::SEEDED_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::SEED_LEFT_EMPTY_REASON,
    severity: GateSeverity::Warn,
};

/// The mover-skew guard: the running mover image predates `spec.seed`, ignored
/// it, and initialized an EMPTY repository. Genuinely terminal and genuinely
/// human-actionable (upgrade the image, delete the empty repository and the
/// terminal bootstrap Job) — the ONE row here that would earn `Fail` on its own
/// merits. It is `Warn` because the registry pins one severity per
/// condition+scope and its siblings must not turn a healthy in-progress seed
/// red; nothing is lost, because a repository in this state is also not `Ready`,
/// which `doctor`'s repository check fails on independently.
pub const SEED_MOVER_TOO_OLD_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::Repository,
    condition: consts::SEEDED_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::SEED_MOVER_TOO_OLD_REASON,
    severity: GateSeverity::Warn,
};

/// A `Restore` whose repository referent does not exist (issue #393): the
/// explicit `spec.repository` object, or the `source.fromPolicy`
/// `SnapshotPolicy` the repository ref is derived from.
///
/// The readiness gate cannot verify a repository it cannot even look up, so the
/// restore parks at `phase: Pending` and — critically — its
/// `policy.waitTimeout` window is NOT opened (`status.waitStartedAt` stays
/// unstamped) until the referent appears and its repository becomes `Ready`.
/// Before #393 the gate fell through unverified here, so a slow-arriving
/// referent silently spent the window; for a `fromPolicy` source, whose
/// `onMissingSnapshot` defaults to `Continue`, a spent window means an EMPTY
/// volume.
///
/// WARN, not Fail, for the reason [`POLICY_REPOSITORY_NOT_READY_GATE`] is:
/// referents applied moments apart by GitOps are the ordinary case and resolve
/// themselves within a requeue or two, so this must explain the park rather
/// than independently turn a diagnostic red. What it must never be is INVISIBLE
/// — a restore parked on a `SnapshotPolicy` that was never applied looks
/// identical, by phase, to one that is simply still resolving.
pub const RESTORE_REFERENT_MISSING_GATE: StructuralGate = StructuralGate {
    applies_to: GateScope::SnapshotOrRestore,
    condition: consts::RESTORE_REFERENT_AVAILABLE_CONDITION,
    blocked_status: CONDITION_FALSE,
    reason: consts::RESTORE_REFERENT_MISSING_REASON,
    severity: GateSeverity::Warn,
};

/// Every human-actionable structural gate kopiur's reconcilers can park an
/// object on.
///
/// Adding a gate to a reconciler means adding a row here; the CLI picks it up
/// with no change, which is the whole point (#359). A condition that merely
/// *reports* health (`IndexBlobHealth`, `BackendReachable`,
/// `SecurityContextCompatible`) is NOT a gate — it blocks nothing — and a
/// time-bounded wait (`SourceStaged`, `PreflightFailed`) is not one either,
/// because it resolves itself into a terminal phase on its own.
///
/// Most rows are phase-INVISIBLE parks (the object sits at `phase: Pending`
/// looking unremarkable), which is what makes the registry load-bearing. One
/// row — [`REPOSITORY_READ_ONLY_GATE`] — is phase-visible and registered
/// anyway, for the explanation it adds to an already-visible failure.
///
/// Each row is also a named `const` so a reconciler can write its condition
/// FROM the row (`io::upsert_gate`) instead of restating the triple at the call
/// site. A row nobody writes, or a writer nobody registered, is caught by the
/// controller's `every_registered_gate_has_a_writer` drift test.
pub const STRUCTURAL_GATES: &[StructuralGate] = &[
    PRIVILEGED_MOVER_GATE,
    MISSING_CREDENTIALS_GATE,
    MISSING_SERVICE_ACCOUNT_GATE,
    MISSING_CA_BUNDLE_GATE,
    DELETION_HELD_GATE,
    MASS_DELETION_HELD_GATE,
    REPOSITORY_READ_ONLY_GATE,
    SCHEDULE_BLOCKED_GATE,
    SCHEDULE_FANOUT_CAPPED_GATE,
    POLICY_REPOSITORY_NOT_READY_GATE,
    SOURCE_PVC_MISSING_GATE,
    RESTORE_REFERENT_MISSING_GATE,
    SEED_SOURCE_NOT_READY_GATE,
    SEED_SOURCE_AUTH_CONFLICT_GATE,
    SEEDING_GATE,
    SEED_SOURCE_NOT_FOUND_GATE,
    SEED_SOURCE_EMPTY_GATE,
    SEED_INCOMPLETE_GATE,
    SEED_LEFT_EMPTY_GATE,
    SEED_MOVER_TOO_OLD_GATE,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_gate_row_is_well_formed() {
        // Tripwire: a row added with an empty/typo'd string, or a polarity that
        // is neither True nor False, would make the gate silently unmatchable
        // on both sides of the contract.
        assert!(
            !STRUCTURAL_GATES.is_empty(),
            "the registry is the shared gate list; an empty one means doctor checks nothing"
        );
        for g in STRUCTURAL_GATES {
            assert!(
                !g.condition.is_empty(),
                "{g:?}: condition must be non-empty"
            );
            assert!(!g.reason.is_empty(), "{g:?}: reason must be non-empty");
            assert!(
                g.blocked_status == CONDITION_TRUE || g.blocked_status == CONDITION_FALSE,
                "{g:?}: blocked_status must be a Kubernetes condition status"
            );
            // The row must match itself and reject the opposite polarity.
            assert!(g.trips(g.condition, g.blocked_status), "{g:?}: self-match");
            let opposite = if g.blocked_status == CONDITION_TRUE {
                CONDITION_FALSE
            } else {
                CONDITION_TRUE
            };
            assert!(
                !g.trips(g.condition, opposite),
                "{g:?}: must not trip on the opposite polarity"
            );
            assert!(
                !g.trips("SomeOtherCondition", g.blocked_status),
                "{g:?}: must not trip on another condition type"
            );
        }
    }

    #[test]
    fn a_live_condition_selects_exactly_one_row() {
        // The whole registry must be a FUNCTION of a live condition: one
        // (type, status, reason) triple in, at most one row out. Rows 2 and 3
        // share condition+status+scope and differ only by reason, so a
        // consumer filtering on `trips` alone would double-report a single
        // wedged Snapshot. `matches` is the matcher that cannot.
        for g in STRUCTURAL_GATES {
            let hits: Vec<_> = STRUCTURAL_GATES
                .iter()
                .filter(|c| c.matches(g.condition, g.blocked_status, g.reason))
                .collect();
            assert_eq!(hits.len(), 1, "{g:?}: must select exactly one row");
            assert_eq!(hits[0], g, "{g:?}: must select ITSELF");
        }

        // The concrete case the reviewer called out, spelled out end to end.
        let creds_secret: Vec<_> = STRUCTURAL_GATES
            .iter()
            .filter(|g| {
                g.matches(
                    consts::CREDENTIALS_AVAILABLE_CONDITION,
                    CONDITION_FALSE,
                    consts::MISSING_CREDENTIALS_REASON,
                )
            })
            .collect();
        assert_eq!(creds_secret.len(), 1);
        assert_eq!(creds_secret[0].reason, consts::MISSING_CREDENTIALS_REASON);

        // ...and the coarse filter deliberately still matches BOTH rows, which
        // is why it must never be used to identify a row.
        let coarse = STRUCTURAL_GATES
            .iter()
            .filter(|g| g.trips(consts::CREDENTIALS_AVAILABLE_CONDITION, CONDITION_FALSE))
            .count();
        assert_eq!(coarse, 3, "`trips` is reason-agnostic by design");

        // An unregistered reason for a gated condition: `matches` finds
        // nothing, `trips` still flags it. That asymmetry is the reason
        // `trips` is kept rather than removed.
        assert!(!STRUCTURAL_GATES.iter().any(|g| g.matches(
            consts::CREDENTIALS_AVAILABLE_CONDITION,
            CONDITION_FALSE,
            "SomeFutureReason"
        )));
        assert!(
            STRUCTURAL_GATES
                .iter()
                .any(|g| g.trips(consts::CREDENTIALS_AVAILABLE_CONDITION, CONDITION_FALSE))
        );
    }

    #[test]
    fn blocked_is_true_mirrors_the_status_string() {
        // The bool a registry-driven `upsert_condition` writer passes. Derived
        // from the row, never hand-translated per call site.
        for g in STRUCTURAL_GATES {
            assert_eq!(
                g.blocked_is_true(),
                g.blocked_status == CONDITION_TRUE,
                "{g:?}"
            );
        }
        // Both polarities are actually exercised by the live registry, so this
        // can never rot into a one-sided assertion.
        assert!(STRUCTURAL_GATES.iter().any(|g| g.blocked_is_true()));
        assert!(STRUCTURAL_GATES.iter().any(|g| !g.blocked_is_true()));
    }

    #[test]
    fn gate_rows_are_unique_and_internally_consistent() {
        // A copy-paste duplicate would double-report the same block. Two rows
        // MAY share a condition+scope when the writer stamps different reasons
        // for it (CredentialsAvailable: missing Secret vs missing SA) — but
        // then they must agree on polarity and severity, or a diagnostic's
        // verdict would depend on which row it happened to match first.
        let mut seen: HashSet<(&str, &str, GateScope)> = HashSet::new();
        for g in STRUCTURAL_GATES {
            assert!(
                seen.insert((g.condition, g.reason, g.applies_to)),
                "{g:?}: duplicate condition+reason+scope row"
            );
        }
        for a in STRUCTURAL_GATES {
            for b in STRUCTURAL_GATES {
                if a.condition == b.condition && a.applies_to == b.applies_to {
                    assert_eq!(
                        a.blocked_status, b.blocked_status,
                        "{a:?} / {b:?}: same condition+scope, different polarity"
                    );
                    assert_eq!(
                        a.severity, b.severity,
                        "{a:?} / {b:?}: same condition+scope, different severity"
                    );
                }
            }
        }
    }

    #[test]
    fn every_seeded_false_reason_this_build_writes_has_a_row() {
        // The `Seeded` condition is the one gated condition with a WIDE reason
        // set, and a partial registration is worse than none: a reason with no
        // row still trips `StructuralGate::trips`, so `kubectl kopiur doctor`
        // classifies it as UNREGISTERED and tells the operator "the operator is
        // newer than the plugin — upgrade the plugin". That would be a false
        // diagnosis handed to someone mid-disaster-recovery, whose actual
        // problem is a mis-pointed seed source.
        //
        // So: every reason this build can stamp on `Seeded=False` must select
        // exactly one row. The list is assembled from the three park/progress
        // reasons plus the shared failure set, so a new reason added to
        // `consts::SEED_FAILURE_REASONS` fails here until it is registered.
        let mut reasons: Vec<&str> = vec![
            consts::WAITING_FOR_SEED_SOURCE_REASON,
            consts::SEED_SOURCE_AUTH_CONFLICT_REASON,
            consts::SEEDING_REASON,
        ];
        reasons.extend_from_slice(consts::SEED_FAILURE_REASONS);
        for reason in reasons {
            let hits: Vec<_> = STRUCTURAL_GATES
                .iter()
                .filter(|g| g.matches(consts::SEEDED_CONDITION, CONDITION_FALSE, reason))
                .collect();
            assert_eq!(
                hits.len(),
                1,
                "Seeded=False reason `{reason}` selects {} rows, not 1 — doctor would report it \
                 as an unknown reason from a newer operator",
                hits.len()
            );
            assert!(hits[0].applies_to.covers_repository());
        }
        // Guard against the reverse rot: every registered `Seeded` row must be
        // one of those reasons, so a row nobody can write cannot linger.
        for g in STRUCTURAL_GATES
            .iter()
            .filter(|g| g.condition == consts::SEEDED_CONDITION)
        {
            assert!(
                g.reason == consts::WAITING_FOR_SEED_SOURCE_REASON
                    || g.reason == consts::SEED_SOURCE_AUTH_CONFLICT_REASON
                    || g.reason == consts::SEEDING_REASON
                    || consts::SEED_FAILURE_REASONS.contains(&g.reason),
                "{g:?}: registered but not a reason this build writes"
            );
        }
    }

    /// Every `Seeded=False` failure reason must be EXPLAINED in the docs, not
    /// just registered (#380).
    ///
    /// The registry above guarantees `kubectl kopiur doctor` recognizes a
    /// reason; it says nothing about whether a human who reads it can find out
    /// what to do. These reasons surface in exactly one situation — a disaster
    /// recovery that is not going well — so an unexplained one is worse here
    /// than almost anywhere else in kopiur. The two pages are the two places
    /// someone actually looks: the troubleshooting table (symptom → fix) and the
    /// DR scenario (the reason table plus the retry/terminal contract).
    ///
    /// A reason added to `consts::SEED_FAILURE_REASONS` — or to the
    /// terminal-until-edited park set the body extends it with — fails here
    /// until both pages mention it by name.
    #[test]
    fn every_seed_failure_reason_is_documented_on_both_user_facing_pages() {
        // CARGO_MANIFEST_DIR = crates/api; the docs tree is at the repo root.
        // Same relative-path approach `crates/api/tests/examples_match_crd_shapes.rs`
        // uses to reach `deploy/examples`.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for page in [
            "docs/troubleshooting.md",
            "docs/scenarios/dr-with-replicated-repository.md",
        ] {
            let path = root.join(page);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{page} must exist and be readable: {e}"));
            // The mover's failure classes, PLUS the one park reason that is
            // equally terminal-until-edited: a migrate-mode workload-identity
            // conflict is re-checked forever and never clears by itself, so
            // doctor prints it at someone who needs the same lookup. The two
            // progress/park reasons that DO clear on their own
            // (`Seeding`, `WaitingForSeedSource`) are deliberately not required
            // here — they describe motion, not a thing to look up.
            let documented: Vec<&str> = consts::SEED_FAILURE_REASONS
                .iter()
                .copied()
                .chain(std::iter::once(consts::SEED_SOURCE_AUTH_CONFLICT_REASON))
                .collect();
            for reason in documented {
                assert!(
                    text.contains(reason),
                    "{page} never mentions the `Seeded=False` reason `{reason}` — doctor will \
                     print it at a user who then has nowhere to look it up"
                );
            }
        }
    }

    #[test]
    fn expected_gates_are_registered_with_expected_scope_and_severity() {
        // Pins the exact contract M3's doctor consumes. A row removed, or its
        // severity/scope quietly changed, fails here rather than silently
        // changing what a cluster diagnostic reports.
        let expected: &[(&str, &str, &str, GateScope, GateSeverity)] = &[
            (
                consts::MOVER_PERMITTED_CONDITION,
                CONDITION_FALSE,
                consts::PRIVILEGED_MOVER_NOT_PERMITTED_REASON,
                GateScope::SnapshotOrRestore,
                GateSeverity::Fail,
            ),
            (
                consts::CREDENTIALS_AVAILABLE_CONDITION,
                CONDITION_FALSE,
                consts::MISSING_CREDENTIALS_REASON,
                GateScope::SnapshotOrRestore,
                GateSeverity::Fail,
            ),
            (
                consts::CREDENTIALS_AVAILABLE_CONDITION,
                CONDITION_FALSE,
                consts::MISSING_SERVICE_ACCOUNT_REASON,
                GateScope::SnapshotOrRestore,
                GateSeverity::Fail,
            ),
            (
                consts::CREDENTIALS_AVAILABLE_CONDITION,
                CONDITION_FALSE,
                consts::MISSING_CA_BUNDLE_REASON,
                GateScope::SnapshotOrRestore,
                GateSeverity::Fail,
            ),
            (
                consts::DELETION_HELD_CONDITION,
                CONDITION_TRUE,
                consts::MASS_DELETION_BREAKER_REASON,
                GateScope::Snapshot,
                GateSeverity::Fail,
            ),
            (
                consts::MASS_DELETION_HELD_CONDITION,
                CONDITION_TRUE,
                consts::MASS_DELETION_THRESHOLD_EXCEEDED_REASON,
                GateScope::Repository,
                GateSeverity::Fail,
            ),
            (
                consts::REPOSITORY_WRITABLE_CONDITION,
                CONDITION_FALSE,
                consts::REPOSITORY_READ_ONLY_REASON,
                GateScope::Snapshot,
                GateSeverity::Warn,
            ),
            (
                consts::SCHEDULE_RUNNABLE_CONDITION,
                CONDITION_FALSE,
                consts::BLOCKED_ON_UNREADABLE_RUN_REASON,
                GateScope::SnapshotSchedule,
                GateSeverity::Fail,
            ),
            (
                consts::SCHEDULE_FANOUT_CAPPED_CONDITION,
                CONDITION_TRUE,
                consts::FANOUT_TOO_LARGE_REASON,
                GateScope::SnapshotSchedule,
                GateSeverity::Fail,
            ),
            (
                consts::REPOSITORIES_READY_CONDITION,
                CONDITION_FALSE,
                consts::REPOSITORY_NOT_READY_REASON,
                GateScope::SnapshotPolicy,
                GateSeverity::Warn,
            ),
            (
                consts::SOURCE_PVC_AVAILABLE_CONDITION,
                CONDITION_FALSE,
                consts::SOURCE_PVC_MISSING_REASON,
                GateScope::Snapshot,
                GateSeverity::Fail,
            ),
            (
                consts::RESTORE_REFERENT_AVAILABLE_CONDITION,
                CONDITION_FALSE,
                consts::RESTORE_REFERENT_MISSING_REASON,
                GateScope::SnapshotOrRestore,
                GateSeverity::Warn,
            ),
            (
                consts::SEEDED_CONDITION,
                CONDITION_FALSE,
                consts::WAITING_FOR_SEED_SOURCE_REASON,
                GateScope::Repository,
                GateSeverity::Warn,
            ),
            (
                consts::SEEDED_CONDITION,
                CONDITION_FALSE,
                consts::SEED_SOURCE_AUTH_CONFLICT_REASON,
                GateScope::Repository,
                GateSeverity::Warn,
            ),
            (
                consts::SEEDED_CONDITION,
                CONDITION_FALSE,
                consts::SEEDING_REASON,
                GateScope::Repository,
                GateSeverity::Warn,
            ),
            (
                consts::SEEDED_CONDITION,
                CONDITION_FALSE,
                consts::SEED_SOURCE_NOT_FOUND_REASON,
                GateScope::Repository,
                GateSeverity::Warn,
            ),
            (
                consts::SEEDED_CONDITION,
                CONDITION_FALSE,
                consts::SEED_SOURCE_EMPTY_REASON,
                GateScope::Repository,
                GateSeverity::Warn,
            ),
            (
                consts::SEEDED_CONDITION,
                CONDITION_FALSE,
                consts::SEED_INCOMPLETE_REASON,
                GateScope::Repository,
                GateSeverity::Warn,
            ),
            (
                consts::SEEDED_CONDITION,
                CONDITION_FALSE,
                consts::SEED_LEFT_EMPTY_REASON,
                GateScope::Repository,
                GateSeverity::Warn,
            ),
            (
                consts::SEEDED_CONDITION,
                CONDITION_FALSE,
                consts::SEED_MOVER_TOO_OLD_REASON,
                GateScope::Repository,
                GateSeverity::Warn,
            ),
        ];
        assert_eq!(
            STRUCTURAL_GATES.len(),
            expected.len(),
            "a gate row was added or removed — update the pinned expectations \
             (and M3's doctor coverage) deliberately"
        );
        for (condition, status, reason, scope, severity) in expected {
            let row = STRUCTURAL_GATES
                .iter()
                .find(|g| g.condition == *condition && g.reason == *reason)
                .unwrap_or_else(|| panic!("{condition}/{reason} must be registered"));
            assert_eq!(row.blocked_status, *status, "{condition}/{reason} polarity");
            assert_eq!(row.applies_to, *scope, "{condition}/{reason} scope");
            assert_eq!(row.severity, *severity, "{condition}/{reason} severity");
        }
    }

    /// The #393 park must be visible to `doctor` on a `Restore` — and must not
    /// make any OTHER condition read as a gate.
    ///
    /// The registry's coarse [`StructuralGate::trips`] filter is
    /// reason-agnostic, so a row registered on a condition other reconcilers
    /// also write (`Ready`, `Resolved`, …) would make every unrelated reason on
    /// that condition report as "a gate from a newer operator". This row's
    /// condition is therefore written by exactly one gate and one reason.
    #[test]
    fn the_restore_referent_gate_is_restore_scoped_and_owns_its_condition() {
        let row = STRUCTURAL_GATES
            .iter()
            .find(|g| g.reason == consts::RESTORE_REFERENT_MISSING_REASON)
            .expect("the #393 referent-missing park must be registered");
        // Reachable from a Restore (the only scope whose `covers_restore` is true).
        assert!(row.applies_to.covers_restore());
        assert_eq!(row.applies_to, GateScope::SnapshotOrRestore);
        // Warn: referents applied moments apart by GitOps resolve themselves.
        assert_eq!(row.severity, GateSeverity::Warn);
        assert_eq!(row.condition, consts::RESTORE_REFERENT_AVAILABLE_CONDITION);
        assert!(row.trips(consts::RESTORE_REFERENT_AVAILABLE_CONDITION, "False"));
        assert!(!row.trips(consts::RESTORE_REFERENT_AVAILABLE_CONDITION, "True"));
        // Exactly one row owns this condition, so the coarse filter can only
        // fire for the one reason this build writes on it.
        assert_eq!(
            STRUCTURAL_GATES
                .iter()
                .filter(|g| g.condition == consts::RESTORE_REFERENT_AVAILABLE_CONDITION)
                .count(),
            1
        );
        // ...and it is NOT the `Ready` condition, which every Restore writes with
        // many reasons — registering there would misreport all of them.
        assert_ne!(row.condition, consts::READY_CONDITION);
        assert!(
            !STRUCTURAL_GATES
                .iter()
                .any(|g| g.condition == consts::READY_CONDITION)
        );
    }

    #[test]
    fn every_scope_classifier_is_consistent() {
        // Each scope covers exactly one of the four kind families a consumer
        // reads from separate lists (work CRs, repositories, schedules,
        // policies), so a diagnostic can dispatch on scope without
        // double-listing a gate.
        for scope in [
            GateScope::SnapshotOrRestore,
            GateScope::Snapshot,
            GateScope::Repository,
            GateScope::SnapshotSchedule,
            GateScope::SnapshotPolicy,
        ] {
            let work = scope.covers_snapshot() || scope.covers_restore();
            let families = [
                work,
                scope.covers_repository(),
                scope.covers_snapshot_schedule(),
                scope.covers_snapshot_policy(),
            ];
            assert_eq!(
                families.iter().filter(|f| **f).count(),
                1,
                "{scope:?} must cover exactly one kind family, got {families:?}"
            );
        }
    }

    #[test]
    fn severity_labels_are_stable_and_distinct() {
        assert_eq!(GateSeverity::Fail.label(), "Fail");
        assert_eq!(GateSeverity::Warn.label(), "Warn");
    }
}
