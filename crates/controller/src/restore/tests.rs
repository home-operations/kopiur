use super::*;
use kopiur_api::common::ObjectRef;
use kopiur_api::restore::{FromPolicy, IdentitySource};

// The repository-derivation tests moved to `kopiur_api::snapshot` with the
// pure fn (`repository_ref_for`); the browse data-plane shares it.

fn job_with_times(start: Option<&str>, end: Option<&str>) -> k8s_openapi::api::batch::v1::Job {
    use k8s_openapi::api::batch::v1::{Job, JobStatus};
    let parse = |s: &str| serde_json::from_value(serde_json::json!(s)).unwrap();
    Job {
        status: Some(JobStatus {
            start_time: start.map(parse),
            completion_time: end.map(parse),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn restore_duration_is_completion_minus_start() {
    let job = job_with_times(Some("2024-01-01T00:00:00Z"), Some("2024-01-01T00:01:30Z"));
    assert_eq!(restore_job_duration_seconds(&job), Some(90));
    // Missing completion → None (still running).
    assert_eq!(
        restore_job_duration_seconds(&job_with_times(Some("2024-01-01T00:00:00Z"), None)),
        None
    );
    // Negative interval (clock skew) → None.
    let skew = job_with_times(Some("2024-01-01T00:01:00Z"), Some("2024-01-01T00:00:00Z"));
    assert_eq!(restore_job_duration_seconds(&skew), None);
}

fn snapshot_ref() -> RestoreSource {
    RestoreSource::SnapshotRef(ObjectRef {
        name: "b".into(),
        namespace: None,
    })
}
fn from_config() -> RestoreSource {
    RestoreSource::FromPolicy(FromPolicy {
        name: "cfg".into(),
        namespace: None,
        as_of: None,
        offset: 0,
    })
}
fn identity() -> RestoreSource {
    RestoreSource::Identity(IdentitySource {
        username: "u".into(),
        hostname: "h".into(),
        source_path: None,
        snapshot_id: None,
        as_of: None,
        offset: None,
    })
}

#[test]
fn from_config_defaults_to_continue_others_fail() {
    assert_eq!(
        default_on_missing(&from_config()),
        OnMissingSnapshot::Continue
    );
    assert_eq!(default_on_missing(&snapshot_ref()), OnMissingSnapshot::Fail);
    assert_eq!(default_on_missing(&identity()), OnMissingSnapshot::Fail);
}

#[test]
fn explicit_on_missing_overrides_default() {
    // fromPolicy would default Continue, but an explicit Fail wins.
    assert_eq!(
        effective_on_missing(Some(OnMissingSnapshot::Fail), &from_config()),
        OnMissingSnapshot::Fail
    );
    // snapshotRef defaults Fail, explicit Continue wins.
    assert_eq!(
        effective_on_missing(Some(OnMissingSnapshot::Continue), &snapshot_ref()),
        OnMissingSnapshot::Continue
    );
}

#[test]
fn source_mode_strings_match_each_variant() {
    assert_eq!(source_mode(&snapshot_ref()), "SnapshotRef");
    assert_eq!(source_mode(&from_config()), "FromPolicy");
    assert_eq!(source_mode(&identity()), "Identity");
}

// `filter_as_of` / `pick_offset` (snapshot selection) moved to
// `kopiur_kopia::selection` with their unit tests — both binaries share them and
// only the mover resolves by-identity now.

#[test]
fn wait_remaining_counts_down_from_creation_and_closes() {
    // 5m window, 60s elapsed → 240s left.
    assert_eq!(wait_remaining_secs(1000, Some("5m"), 1060), Some(240));
    // Window exactly elapsed → closed (None), onMissingSnapshot applies.
    assert_eq!(wait_remaining_secs(1000, Some("5m"), 1300), None);
    assert_eq!(wait_remaining_secs(1000, Some("5m"), 1301), None);
    // No waitTimeout configured → no window at all.
    assert_eq!(wait_remaining_secs(1000, None, 1000), None);
    // Unparseable timeout → treated as no window (webhook rejects it at
    // admission; this is the defensive path).
    assert_eq!(wait_remaining_secs(1000, Some("bogus"), 1000), None);
}

#[test]
fn populator_state_depends_on_target_variant() {
    use kopiur_api::PopulatorTarget;
    use kopiur_api::common::ObjectRef;
    use kopiur_api::restore::PvcTemplate;
    // populator target → passive AwaitingClaim.
    assert_eq!(
        populator_state(&RestoreTarget::Populator(PopulatorTarget {})),
        PopulatorState::AwaitingClaim
    );
    // explicit pvc/pvcRef → operator-driven DirectTarget.
    assert_eq!(
        populator_state(&RestoreTarget::PvcRef(ObjectRef {
            name: "data".into(),
            namespace: None,
        })),
        PopulatorState::DirectTarget
    );
    assert_eq!(
        populator_state(&RestoreTarget::Pvc(PvcTemplate {
            name: "created".into(),
            storage_class_name: None,
            capacity: None,
            access_modes: vec![],
        })),
        PopulatorState::DirectTarget
    );
}

#[test]
fn populator_completed_is_not_terminal_at_guard() {
    use PopulatorState::{AwaitingClaim, DirectTarget};
    use RestorePhase::{Completed, Failed, Pending, Resolving, Restoring};

    // A populator `Completed` (mover done with the prime PVC, rebind still pending)
    // must NOT be terminal at the guard, or the rebind never runs.
    assert!(!phase_is_terminal_at_guard(Completed, AwaitingClaim));
    // A direct restore writes the target itself, so `Completed` IS terminal.
    assert!(phase_is_terminal_at_guard(Completed, DirectTarget));
    // `Failed` is terminal regardless of dispatch model.
    assert!(phase_is_terminal_at_guard(Failed, AwaitingClaim));
    assert!(phase_is_terminal_at_guard(Failed, DirectTarget));
    // In-flight phases are never terminal.
    for p in [Pending, Resolving, Restoring] {
        assert!(!phase_is_terminal_at_guard(p, AwaitingClaim));
        assert!(!phase_is_terminal_at_guard(p, DirectTarget));
    }
}

fn resolved_with(
    resolution: Option<ResolutionOutcome>,
    kopia_snapshot_id: Option<&str>,
) -> ResolvedRestore {
    ResolvedRestore {
        resolution,
        kopia_snapshot_id: kopia_snapshot_id.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn pinned_decision_reads_the_pinned_outcome_and_never_re_resolves() {
    use PopulatorState::{AwaitingClaim, DirectTarget};
    use RestorePhase::{Completed, Pending};

    // A pinned `NoSnapshot` is always the deploy-or-restore Empty decision — even
    // if a kopiaSnapshotID somehow co-exists, NoSnapshot wins (data-safety: a later
    // snapshot must never retarget a volume that already came up empty).
    assert_eq!(
        pinned_decision(
            Some(&resolved_with(Some(ResolutionOutcome::NoSnapshot), None)),
            Some(Completed),
            AwaitingClaim,
        ),
        Some(Resolution::Empty)
    );

    // A pinned snapshot id resolves to that id (with the explicit Snapshot outcome…).
    assert_eq!(
        pinned_decision(
            Some(&resolved_with(
                Some(ResolutionOutcome::Snapshot),
                Some("k7")
            )),
            Some(Pending),
            DirectTarget,
        ),
        Some(Resolution::Snapshot("k7".into()))
    );
    // …and a LEGACY pin (id present, `resolution` field absent) reads the same,
    // so an in-flight restore pinned before this field existed keeps its target.
    assert_eq!(
        pinned_decision(
            Some(&resolved_with(None, Some("k7"))),
            Some(Pending),
            DirectTarget,
        ),
        Some(Resolution::Snapshot("k7".into()))
    );

    // The pre-fix stuck populator: `Completed` with NOTHING pinned. A snapshot-
    // resolved populator ALWAYS pins before Completed, so this unambiguously means
    // the decision was "empty" — back-fill Empty, do NOT re-resolve.
    assert_eq!(
        pinned_decision(None, Some(Completed), AwaitingClaim),
        Some(Resolution::Empty)
    );
    // The same shape on a DIRECT target is not a stuck populator (its `Completed`
    // is terminal at the guard, so it never reaches here): require fresh resolution.
    assert_eq!(pinned_decision(None, Some(Completed), DirectTarget), None);

    // A fresh, un-pinned restore must resolve.
    assert_eq!(pinned_decision(None, Some(Pending), AwaitingClaim), None);
    assert_eq!(pinned_decision(None, None, DirectTarget), None);
}

// --- kstatus Ready conditions (ADR-0005 §2) -----------------------------
// Regression: the job-terminal transitions used to write the phase ALONE
// (no conditions), so `kubectl wait --for=condition=Ready` and Flux
// healthChecks could never gate on a Completed Restore; and the
// missing-snapshot/awaiting-claim patches replaced the whole conditions
// array, dropping domain conditions set earlier.

#[test]
fn ready_outcome_maps_every_phase() {
    use crate::io::ReadyOutcome;
    assert_eq!(
        restore_ready_outcome(RestorePhase::Completed),
        ReadyOutcome::Ready
    );
    assert_eq!(
        restore_ready_outcome(RestorePhase::Failed),
        ReadyOutcome::Stalled
    );
    for p in [
        RestorePhase::Pending,
        RestorePhase::Resolving,
        RestorePhase::Restoring,
    ] {
        assert_eq!(restore_ready_outcome(p), ReadyOutcome::Reconciling, "{p:?}");
    }
}

/// A minimal Restore with `generation: 3` and one pre-existing condition,
/// parsed the cluster's way (JSON → typed).
fn restore_with_condition(type_: &str, status: &str) -> Restore {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": "r", "namespace": "ns", "generation": 3 },
        "spec": {
            "source": { "snapshotRef": { "name": "b" } },
            "target": { "pvcRef": { "name": "t" } }
        },
        "status": { "conditions": [{
            "type": type_, "status": status, "reason": "X", "message": "m",
            "lastTransitionTime": "2026-01-01T00:00:00Z"
        }] }
    }))
    .expect("valid Restore")
}

fn cond<'a>(v: &'a serde_json::Value, type_: &str) -> &'a serde_json::Value {
    v["conditions"]
        .as_array()
        .expect("conditions array")
        .iter()
        .find(|c| c["type"] == type_)
        .unwrap_or_else(|| panic!("missing condition {type_}"))
}

#[test]
fn ready_status_completed_sets_ready_and_preserves_domain_conditions() {
    let r = restore_with_condition("Resolved", "True");
    let v = restore_ready_status(&r, RestorePhase::Completed, "RestoreSucceeded", "done");
    assert_eq!(v["phase"], "Completed");
    assert_eq!(v["observedGeneration"], 3);
    assert_eq!(cond(&v, "Ready")["status"], "True");
    assert_eq!(cond(&v, "Ready")["reason"], "RestoreSucceeded");
    assert_eq!(cond(&v, "Reconciling")["status"], "False");
    assert_eq!(cond(&v, "Stalled")["status"], "False");
    // The pre-existing domain condition survives the phase write (the old
    // bare-array patches dropped it).
    assert_eq!(cond(&v, "Resolved")["status"], "True");
}

#[test]
fn ready_status_failed_is_stalled_not_ready() {
    let r = restore_with_condition("MoverPermitted", "True");
    let v = restore_ready_status(
        &r,
        RestorePhase::Failed,
        "MoverJobFailed",
        "the restore mover Job failed",
    );
    assert_eq!(v["phase"], "Failed");
    assert_eq!(cond(&v, "Ready")["status"], "False");
    assert_eq!(cond(&v, "Stalled")["status"], "True");
    assert_eq!(cond(&v, "Stalled")["reason"], "MoverJobFailed");
    assert_eq!(cond(&v, "MoverPermitted")["status"], "True");
}

/// The mover-stamp race the e2e caught live: the mover PATCHes
/// `phase: Completed` (no conditions) before the controller's Job-terminal
/// transition runs, so the object sits terminal with the in-flight trio
/// (`Ready=False reason=MoverJobCreated`). The terminal gate must detect
/// that as NOT settled and heal; once healed it must read as settled (the
/// self-gate that stops re-patching).
#[test]
fn mover_stamped_terminal_phase_without_ready_is_not_settled() {
    let mut r = restore_with_condition("Resolved", "True");
    // In-flight trio, as written by the MoverJobCreated transition.
    let inflight = io::set_ready(
        &r.status.as_ref().unwrap().conditions,
        r.metadata.generation,
        io::ReadyOutcome::Reconciling,
        "MoverJobCreated",
        "created the restore mover Job",
    );
    let mut status = r.status.take().unwrap();
    status.conditions = inflight;
    status.phase = Some(RestorePhase::Completed); // mover stamp: phase only
    r.status = Some(status);

    assert!(!kstatus_settled_for(&r, RestorePhase::Completed));
    assert!(!kstatus_settled_for(&r, RestorePhase::Failed));

    // Heal (what the terminal gate patches), then it must be settled.
    let healed = restore_ready_status(&r, RestorePhase::Completed, "RestoreSucceeded", "done");
    let mut status = r.status.take().unwrap();
    status.conditions = serde_json::from_value(healed["conditions"].clone()).unwrap();
    r.status = Some(status);
    assert!(kstatus_settled_for(&r, RestorePhase::Completed));
    // ...and the domain condition still survives the heal.
    let conds = &r.status.as_ref().unwrap().conditions;
    assert!(
        conds
            .iter()
            .any(|c| c.type_ == "Resolved" && c.status == "True")
    );
}

#[test]
fn ready_status_in_flight_is_reconciling() {
    let r = restore_with_condition("Resolved", "True");
    let v = restore_ready_status(
        &r,
        RestorePhase::Restoring,
        "MoverJobRunning",
        "the restore mover Job is in flight",
    );
    assert_eq!(v["phase"], "Restoring");
    assert_eq!(cond(&v, "Ready")["status"], "False");
    assert_eq!(cond(&v, "Reconciling")["status"], "True");
    assert_eq!(cond(&v, "Reconciling")["reason"], "MoverJobRunning");
    assert_eq!(cond(&v, "Stalled")["status"], "False");
}

fn pvc(value: serde_json::Value) -> k8s_openapi::api::core::v1::PersistentVolumeClaim {
    serde_json::from_value(value).unwrap()
}

#[test]
fn pvc_claims_restore_matches_only_our_datasourceref() {
    let claim = pvc(serde_json::json!({
        "metadata": { "name": "qui", "namespace": "downloads" },
        "spec": { "dataSourceRef": {
            "apiGroup": "kopiur.home-operations.com", "kind": "Restore", "name": "qui",
        } },
    }));
    assert!(pvc_claims_restore(&claim, "qui"));
    assert!(!pvc_claims_restore(&claim, "other"));

    // Wrong apiGroup (a VolSync ReplicationDestination) must not match.
    let volsync = pvc(serde_json::json!({
        "metadata": { "name": "qui", "namespace": "downloads" },
        "spec": { "dataSourceRef": {
            "apiGroup": "volsync.backube", "kind": "ReplicationDestination", "name": "qui",
        } },
    }));
    assert!(!pvc_claims_restore(&volsync, "qui"));

    // No dataSourceRef at all.
    let plain = pvc(serde_json::json!({ "metadata": { "name": "qui" }, "spec": {} }));
    assert!(!pvc_claims_restore(&plain, "qui"));
}

#[test]
fn pvc_is_bound_reads_volume_name_or_phase() {
    assert!(pvc_is_bound(&pvc(serde_json::json!({
        "metadata": { "name": "p" }, "spec": { "volumeName": "pvc-123" },
    }))));
    assert!(pvc_is_bound(&pvc(serde_json::json!({
        "metadata": { "name": "p" }, "spec": {}, "status": { "phase": "Bound" },
    }))));
    assert!(!pvc_is_bound(&pvc(serde_json::json!({
        "metadata": { "name": "p" }, "spec": {}, "status": { "phase": "Pending" },
    }))));
}
