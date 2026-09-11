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
use crate::api::gate_severity_view;

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
/// `severity` goes through [`gate_severity_view`], the same projection a live
/// [`GateHit`](kopiur_ui_model::graph::GateHit) uses, so this endpoint's
/// documentation of a gate and the hit the SPA receives when it fires describe
/// it identically. They once did not: `/gates` said `error` where a hit said
/// `Fail`, for the same registry row.
pub fn descriptor(gate: &StructuralGate) -> GateDescriptor {
    GateDescriptor {
        scope: scope_label(gate.applies_to).to_string(),
        condition: gate.condition.to_string(),
        blocked_status: gate.blocked_status.to_string(),
        reason: gate.reason.to_string(),
        severity: gate_severity_view(gate.severity),
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
    fn both_severity_levels_actually_occur_in_the_registry() {
        use kopiur_ui_model::graph::GateSeverityView;
        // The enum makes "which vocabulary?" unrepresentable, so what is left to
        // pin is that both levels are real — a registry that only ever said
        // `Error` would leave the distinction untested and free to rot.
        let all = view_all();
        assert!(all.iter().any(|g| g.severity == GateSeverityView::Error));
        assert!(all.iter().any(|g| g.severity == GateSeverityView::Warning));
    }

    #[test]
    fn a_descriptors_severity_is_the_one_a_live_hit_of_that_gate_reports() {
        // The A-C1 regression guard: `/gates` documenting a severity the
        // `gates: [...]` arrays contradict is exactly what shipped before.
        for gate in STRUCTURAL_GATES {
            assert_eq!(
                descriptor(gate).severity,
                gate_severity_view(gate.severity),
                "/gates must document {} with the severity a live hit reports",
                gate.condition
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
