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
}

impl GateScope {
    /// Whether a `Snapshot` can carry a gate of this scope. Exhaustive.
    pub fn covers_snapshot(self) -> bool {
        match self {
            Self::SnapshotOrRestore | Self::Snapshot => true,
            Self::Repository => false,
        }
    }

    /// Whether a `Restore` can carry a gate of this scope. Exhaustive.
    pub fn covers_restore(self) -> bool {
        match self {
            Self::SnapshotOrRestore => true,
            Self::Snapshot | Self::Repository => false,
        }
    }

    /// Whether a `Repository`/`ClusterRepository` can carry a gate of this
    /// scope. Exhaustive.
    pub fn covers_repository(self) -> bool {
        match self {
            Self::Repository => true,
            Self::SnapshotOrRestore | Self::Snapshot => false,
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
    /// Whether a live condition's `type`/`status` pair trips this gate.
    ///
    /// The polarity comparison lives here so no consumer re-derives it (the
    /// `!= "True"` / `== "True"` mix-ups this registry exists to prevent).
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
    /// ```
    pub fn trips(&self, condition_type: &str, status: &str) -> bool {
        condition_type == self.condition && status == self.blocked_status
    }
}

/// Every human-actionable structural gate kopiur's reconcilers can park an
/// object on.
///
/// Adding a gate to a reconciler means adding a row here; the CLI picks it up
/// with no change, which is the whole point (#359). A condition that merely
/// *reports* health (`IndexBlobHealth`, `BackendReachable`,
/// `SecurityContextCompatible`) is NOT a gate — it blocks nothing — and a
/// time-bounded wait (`SourceStaged`, `PreflightFailed`) is not one either,
/// because it resolves itself into a terminal phase that phase-based checks
/// already see.
pub const STRUCTURAL_GATES: &[StructuralGate] = &[
    // An elevated mover in a namespace that has not opted in. The admin adds
    // the `privileged-movers` annotation out-of-band; until then the object
    // sits at `phase: Pending` — the exact shape of #359.
    StructuralGate {
        applies_to: GateScope::SnapshotOrRestore,
        condition: consts::MOVER_PERMITTED_CONDITION,
        blocked_status: CONDITION_FALSE,
        reason: consts::PRIVILEGED_MOVER_NOT_PERMITTED_REASON,
        severity: GateSeverity::Fail,
    },
    // The mover's credential Secret is not in the workload namespace. Parks at
    // `phase: Pending` until the user creates it (or enables projection).
    StructuralGate {
        applies_to: GateScope::SnapshotOrRestore,
        condition: consts::CREDENTIALS_AVAILABLE_CONDITION,
        blocked_status: CONDITION_FALSE,
        reason: consts::MISSING_CREDENTIALS_REASON,
        severity: GateSeverity::Fail,
    },
    // Same condition, different missing dependency: the workload-identity
    // ServiceAccount the backend names. kopiur never creates it.
    StructuralGate {
        applies_to: GateScope::SnapshotOrRestore,
        condition: consts::CREDENTIALS_AVAILABLE_CONDITION,
        blocked_status: CONDITION_FALSE,
        reason: consts::MISSING_SERVICE_ACCOUNT_REASON,
        severity: GateSeverity::Fail,
    },
    // Inverted polarity: the per-Snapshot hold the mass-deletion breaker
    // applies. Released only by the `allow-mass-deletion` acknowledgement on
    // the repository, so it is squarely "needs a human".
    StructuralGate {
        applies_to: GateScope::Snapshot,
        condition: consts::DELETION_HELD_CONDITION,
        blocked_status: CONDITION_TRUE,
        reason: consts::MASS_DELETION_BREAKER_REASON,
        severity: GateSeverity::Fail,
    },
    // The repository-level view of the same breaker: a whole wave is held.
    StructuralGate {
        applies_to: GateScope::Repository,
        condition: consts::MASS_DELETION_HELD_CONDITION,
        blocked_status: CONDITION_TRUE,
        reason: consts::MASS_DELETION_THRESHOLD_EXCEEDED_REASON,
        severity: GateSeverity::Fail,
    },
    // A backup refused because its repository is `mode: ReadOnly`. WARN, not
    // Fail: a read-only repository is a legitimate, deliberate configuration
    // (a replication target, an archived repo served for restores only), so a
    // green/red verdict must not hinge on it — but it still explains why a
    // backup will never run, which is worth saying out loud.
    StructuralGate {
        applies_to: GateScope::Snapshot,
        condition: consts::REPOSITORY_WRITABLE_CONDITION,
        blocked_status: CONDITION_FALSE,
        reason: consts::REPOSITORY_READ_ONLY_REASON,
        severity: GateSeverity::Warn,
    },
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

    #[test]
    fn every_scope_classifier_is_consistent() {
        // Each scope covers at least one kind, and no scope claims both a work
        // kind and the repository kind (they are read from different lists).
        for scope in [
            GateScope::SnapshotOrRestore,
            GateScope::Snapshot,
            GateScope::Repository,
        ] {
            let work = scope.covers_snapshot() || scope.covers_restore();
            assert!(
                work || scope.covers_repository(),
                "{scope:?} covers no kind at all"
            );
            assert!(
                !(work && scope.covers_repository()),
                "{scope:?} straddles work and repository kinds"
            );
        }
    }

    #[test]
    fn severity_labels_are_stable_and_distinct() {
        assert_eq!(GateSeverity::Fail.label(), "Fail");
        assert_eq!(GateSeverity::Warn.label(), "Warn");
    }
}
