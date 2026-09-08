//! `GET /api/v1/gates` — the typed gate-condition registry
//! ([`kopiur_api::gates`]), so the SPA explains a parked resource in the same
//! words the operator uses.
//!
//! # Why the whole registry, not just the gates that have fired
//!
//! A structural gate never self-heals: the object parks and waits for a human.
//! The UI has to be able to explain one it has never seen — including a gate a
//! *newer* operator raised — and the only way to do that without a lookup table
//! that drifts is to publish the registry both sides already share. There is no
//! IO here at all; the answer is a compile-time constant projected onto the wire.

use axum::{Json, Router, routing::get};

use kopiur_api::gates::{GateScope, STRUCTURAL_GATES, StructuralGate};
use kopiur_ui_model::views::GateDescriptor;

use crate::AppState;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new().route("/gates", get(handler))
}

/// **Pure.** The CRD kinds a gate's condition appears on, as one display string.
///
/// Exhaustive over [`GateScope`], so a new scope must state how it reads before
/// it compiles.
fn scope_label(scope: GateScope) -> &'static str {
    match scope {
        GateScope::SnapshotOrRestore => "Snapshot,Restore",
        GateScope::Snapshot => "Snapshot",
        GateScope::Repository => "Repository,ClusterRepository",
        GateScope::SnapshotSchedule => "SnapshotSchedule",
        GateScope::SnapshotPolicy => "SnapshotPolicy",
    }
}

/// **Pure.** One registry row as its wire descriptor.
///
/// `severity` is lower-cased to the `info`/`warning`/`error` vocabulary the wire
/// type documents; the registry itself has only two levels, so `info` is
/// unused — a gate is either wedged work or a plausible deliberate refusal, and
/// neither is merely informational.
pub fn descriptor(gate: &StructuralGate) -> GateDescriptor {
    use kopiur_api::gates::GateSeverity;
    GateDescriptor {
        scope: scope_label(gate.applies_to).to_string(),
        condition: gate.condition.to_string(),
        blocked_status: gate.blocked_status.to_string(),
        reason: gate.reason.to_string(),
        severity: match gate.severity {
            GateSeverity::Fail => "error",
            GateSeverity::Warn => "warning",
        }
        .to_string(),
    }
}

/// **Pure.** The whole registry.
pub fn view_all() -> Vec<GateDescriptor> {
    STRUCTURAL_GATES.iter().map(descriptor).collect()
}

/// `GET /api/v1/gates`
async fn handler() -> Json<Vec<GateDescriptor>> {
    Json(view_all())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_gate_is_published() {
        let all = view_all();
        assert_eq!(
            all.len(),
            STRUCTURAL_GATES.len(),
            "the endpoint is the registry, not a subset of it"
        );
        assert!(!all.is_empty(), "the registry is not empty");
    }

    #[test]
    fn a_descriptor_carries_the_triple_that_identifies_a_live_condition() {
        let mover = view_all()
            .into_iter()
            .find(|g| g.condition == "MoverPermitted")
            .expect("the privileged-mover gate is registered");
        assert_eq!(mover.scope, "Snapshot,Restore");
        assert_eq!(
            mover.blocked_status, "False",
            "polarity is per-gate and must travel with it"
        );
        assert_eq!(mover.reason, "PrivilegedMoverNotPermitted");
    }

    #[test]
    fn both_polarities_are_represented_so_the_spa_cannot_assume_one() {
        let all = view_all();
        assert!(
            all.iter().any(|g| g.blocked_status == "False"),
            "some gates block on False"
        );
        assert!(
            all.iter().any(|g| g.blocked_status == "True"),
            "and some block on True — DeletionHeld is the canonical one"
        );
    }

    #[test]
    fn severity_uses_the_wire_vocabulary() {
        for gate in view_all() {
            assert!(
                gate.severity == "error" || gate.severity == "warning",
                "{} has severity {}, which the SPA cannot render",
                gate.condition,
                gate.severity
            );
        }
    }

    #[test]
    fn every_scope_has_a_label() {
        // A `GateScope` with no label would be a compile error in `scope_label`;
        // this pins that each label is distinct, so two scopes cannot silently
        // render as one.
        let labels = [
            scope_label(GateScope::SnapshotOrRestore),
            scope_label(GateScope::Snapshot),
            scope_label(GateScope::Repository),
            scope_label(GateScope::SnapshotSchedule),
            scope_label(GateScope::SnapshotPolicy),
        ];
        let mut unique: Vec<&str> = labels.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "scope labels must be distinct");
    }
}
