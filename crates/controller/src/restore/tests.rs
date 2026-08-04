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
fn readiness_gate_holds_only_pre_launch_phases() {
    use RestorePhase::{Completed, Failed, Pending, Resolving, Restoring};
    // Not yet launched (no status, Pending, or resolved-but-undispatched): the
    // repository-readiness gate may hold these.
    assert!(restore_awaiting_launch(None));
    assert!(restore_awaiting_launch(Some(Pending)));
    assert!(restore_awaiting_launch(Some(Resolving)));
    // A live (or just-terminal) mover Job must be observed, never re-gated —
    // and a populator's non-terminal `Completed` heartbeat must not be flipped
    // back to `Pending`.
    assert!(!restore_awaiting_launch(Some(Restoring)));
    assert!(!restore_awaiting_launch(Some(Completed)));
    assert!(!restore_awaiting_launch(Some(Failed)));
}

#[test]
fn repository_not_ready_restore_message_says_what_why_how() {
    let msg = repository_not_ready_restore_message("nas");
    // What is being waited on, why, and that it self-resolves.
    assert!(msg.contains("`nas`"));
    assert!(msg.contains("`Ready`"));
    assert!(msg.contains("restore"));
    assert!(msg.contains("reconnect"));
    // Mirrors the Snapshot gate's reason constant.
    assert_eq!(
        crate::consts::REPOSITORY_NOT_READY_REASON,
        "RepositoryNotReady"
    );
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
    // must NOT be terminal at the guard, or the rebind never runs. This non-terminal
    // `Completed` is also what makes a populator `Restore` REUSABLE: delete the claiming
    // PVC and apply a fresh one with the same `dataSourceRef` and reconcile falls through
    // here to populate the new (unbound) claim, rather than short-circuiting as "consumed".
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
            false,
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
            false,
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
            false,
        ),
        Some(Resolution::Snapshot("k7".into()))
    );

    // The pre-fix stuck populator: `Completed` with NOTHING pinned. A snapshot-
    // resolved populator ALWAYS pins before Completed, so this unambiguously means
    // the decision was "empty" — back-fill Empty, do NOT re-resolve.
    assert_eq!(
        pinned_decision(None, Some(Completed), AwaitingClaim, false),
        Some(Resolution::Empty)
    );
    // The same shape on a DIRECT target is not a stuck populator (its `Completed`
    // is terminal at the guard, so it never reaches here): require fresh resolution.
    assert_eq!(
        pinned_decision(None, Some(Completed), DirectTarget, false),
        None
    );

    // A fresh, un-pinned restore must resolve.
    assert_eq!(
        pinned_decision(None, Some(Pending), AwaitingClaim, false),
        None
    );
    assert_eq!(pinned_decision(None, None, DirectTarget, false), None);
}

/// #233: the OTHER way a populator reaches `Completed` unpinned is an already-bound
/// no-op on a DEFERRED source — the mover (which pins a deferred source) never ran.
/// Back-filling `Empty` there would durably pin `NoSnapshot`, so a later, legitimate
/// re-creation of the claiming PVC would provision an EMPTY volume instead of restoring
/// the snapshot. The `noop_already_bound` flag must suppress exactly that back-fill —
/// and nothing else.
#[test]
fn pinned_decision_skips_empty_backfill_after_already_bound_noop() {
    use PopulatorState::AwaitingClaim;
    use RestorePhase::Completed;

    // The no-op'd populator: do NOT infer "empty", leave it unresolved so a recreated
    // claim re-resolves and restores for real.
    assert_eq!(
        pinned_decision(None, Some(Completed), AwaitingClaim, true),
        None
    );
    // The legacy stuck populator (same shape, but NOT an already-bound no-op) still
    // back-fills — that heal must survive this fix.
    assert_eq!(
        pinned_decision(None, Some(Completed), AwaitingClaim, false),
        Some(Resolution::Empty)
    );
    // A genuine deploy-or-restore PINNED `NoSnapshot`, so it reads its pin either way:
    // the flag never overrides a real pin.
    for noop in [true, false] {
        assert_eq!(
            pinned_decision(
                Some(&resolved_with(Some(ResolutionOutcome::NoSnapshot), None)),
                Some(Completed),
                AwaitingClaim,
                noop,
            ),
            Some(Resolution::Empty)
        );
        // …and a pinned snapshot id is likewise honored, so a recreated claim restores
        // the SAME snapshot (ADR §4.6: pinned once, never re-resolved).
        assert_eq!(
            pinned_decision(
                Some(&resolved_with(
                    Some(ResolutionOutcome::Snapshot),
                    Some("k9")
                )),
                Some(Completed),
                AwaitingClaim,
                noop,
            ),
            Some(Resolution::Snapshot("k9".into()))
        );
    }
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

// --- #233: the populator handshake verdict --------------------------------
// The bug: a `Restore` re-created (GitOps prune + re-apply) over a claim that is
// ALREADY bound used to provision a prime PVC and run a full restore into it, then
// park forever — the prime could never be adopted (a CSI populator only hands volumes
// to UNBOUND claims), so it sat `Bound` holding a complete copy of the data. Every
// binding ordering is decided here, exhaustively, in one pure place.

#[test]
fn populator_handshake_covers_every_binding_ordering() {
    let unbound = pvc(serde_json::json!({ "metadata": { "name": "c" }, "spec": {} }));
    let bound_ours = pvc(serde_json::json!({
        "metadata": { "name": "c" }, "spec": { "volumeName": "pv-ours" },
    }));
    let bound_foreign = pvc(serde_json::json!({
        "metadata": { "name": "c" }, "spec": { "volumeName": "pv-theirs" },
    }));
    // Bound only through `status.phase` — `spec.volumeName` not observed yet.
    let bound_by_phase = pvc(serde_json::json!({
        "metadata": { "name": "c" }, "spec": {}, "status": { "phase": "Bound" },
    }));

    // No rebind of ours + unbound claim → the normal populate path (also the WFFC
    // shape before a pod schedules the claim).
    assert_eq!(
        populator_handshake(&unbound, None),
        PopulatorHandshake::Populate
    );

    // THE #233 CASE: no rebind of ours + an already-bound claim → nothing to populate.
    assert_eq!(
        populator_handshake(&bound_foreign, None),
        PopulatorHandshake::NothingToPopulate
    );
    // …including a claim that only reads bound through its phase.
    assert_eq!(
        populator_handshake(&bound_by_phase, None),
        PopulatorHandshake::NothingToPopulate
    );

    // Mid-handover: our rebind is issued but the claim has not bound yet. This is the
    // guard that must NOT misfire — reaping here would kill a healthy restore.
    assert_eq!(
        populator_handshake(&unbound, Some("pv-ours")),
        PopulatorHandshake::AwaitingBind
    );
    // Bound-by-phase-only WITH our rebind outstanding is still mid-handover, NOT a lost
    // rebind: `spec.volumeName` is the only field that says WHICH volume won the claim.
    assert_eq!(
        populator_handshake(&bound_by_phase, Some("pv-ours")),
        PopulatorHandshake::AwaitingBind
    );

    // The handover landed → finalize (restore the PV's reclaim policy, GC the prime).
    assert_eq!(
        populator_handshake(&bound_ours, Some("pv-ours")),
        PopulatorHandshake::FinalizeRebound {
            pv: "pv-ours".into()
        }
    );

    // Our rebind was issued but a DIFFERENT PV won the claim: the handover is lost and
    // can never complete. Reap — and keep our PV, which holds the restored data.
    assert_eq!(
        populator_handshake(&bound_foreign, Some("pv-ours")),
        PopulatorHandshake::LostRebind {
            pv: "pv-ours".into()
        }
    );
}

/// A populator Restore that is `Completed` with `Ready=True/<reason>`, parsed the
/// cluster's way (JSON → typed).
fn completed_populator_with_ready_reason(reason: &str) -> Restore {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": "r", "namespace": "ns", "generation": 1 },
        "spec": {
            "source": { "snapshotRef": { "name": "b" } },
            "target": { "populator": {} }
        },
        "status": {
            "phase": "Completed",
            "conditions": [{
                "type": "Ready", "status": "True", "reason": reason, "message": "m",
                "lastTransitionTime": "2026-01-01T00:00:00Z"
            }]
        }
    }))
    .expect("valid Restore")
}

/// The `Ready` reason is what tells the three `Completed` populator states apart, so it
/// gates both the re-resolution skip and the `pinned_decision` back-fill.
#[test]
fn completed_as_target_already_bound_reads_ready_reason() {
    assert!(completed_as_target_already_bound(
        &completed_populator_with_ready_reason(crate::consts::RESTORE_TARGET_ALREADY_BOUND_REASON)
    ));
    // A real restore, and the legacy stuck-populator state, must NOT be mistaken for it —
    // the first would have its success message clobbered, the second would never heal.
    assert!(!completed_as_target_already_bound(
        &completed_populator_with_ready_reason(crate::consts::RESTORE_POPULATED_REASON)
    ));
    assert!(!completed_as_target_already_bound(
        &completed_populator_with_ready_reason("PopulatingPrimePvc")
    ));
    // No status at all (a fresh CR) is not a no-op completion either.
    assert!(!completed_as_target_already_bound(&restore_with_condition(
        "Resolved", "True"
    )));
}

/// The no-op and reap messages are what a human reads when 49 prime PVCs vanish, so the
/// what/why/fix text is asserted like any other behavior.
#[test]
fn target_already_bound_messages_say_what_why_fix() {
    let msg = target_already_bound_message("plex-config", Some("pvc-abc"));
    assert!(msg.contains("`plex-config`"), "{msg}");
    assert!(msg.contains("already bound"), "{msg}");
    assert!(msg.contains("PersistentVolume `pvc-abc`"), "{msg}");
    // The fix: re-create the CLAIM (deleting the Restore just re-triggers this no-op).
    assert!(msg.contains("delete the PVC"), "{msg}");
    // Never claim a restore ran.
    assert!(msg.contains("no restore ran"), "{msg}");
    // A claim bound without an observed volumeName still reads sensibly.
    assert!(
        target_already_bound_message("plex-config", None).contains("a PersistentVolume"),
        "unnamed volume must not render as an empty backtick pair"
    );

    let note =
        reaped_populate_artifacts_note(&["prime PVC `prime-9f2`".to_string()], "plex-config", None);
    assert!(note.contains("prime PVC `prime-9f2`"), "{note}");
    assert!(note.contains("plex-config"), "{note}");
    // A lost rebind must say the volume was KEPT — the data is in there.
    let kept = reaped_populate_artifacts_note(
        &["populate Job `plex-populate`".to_string()],
        "plex-config",
        Some("pv-xyz"),
    );
    assert!(kept.contains("pv-xyz"), "{kept}");
    assert!(kept.contains("Retain"), "{kept}");
    assert!(kept.contains("KEPT"), "{kept}");
}

/// A LOST rebind is not an already-bound no-op: a prime WAS provisioned, a restore DID run,
/// and a full-size volume is now `Retain`ed. Telling the operator "nothing was provisioned,
/// no restore ran" there would hide storage they have just become responsible for.
#[test]
fn lost_rebind_message_never_claims_nothing_ran() {
    let msg = lost_rebind_message("plex-config", "pv-ours");
    assert!(msg.contains("`plex-config`"), "{msg}");
    assert!(msg.contains("pv-ours"), "{msg}");
    assert!(msg.contains("Retain"), "{msg}");
    assert!(
        !msg.contains("no restore ran"),
        "a lost rebind DID run a restore: {msg}"
    );
    assert!(
        msg.contains("restored data is NOT in the claim"),
        "must say where the data actually is: {msg}"
    );
}

/// A claim bound out from under a RUNNING populate is a hijacked handover, not a success:
/// the app is about to start on someone else's (probably empty) volume, so reporting
/// `Ready=True` would tell `kubectl wait`/Flux a restore landed when it did not.
#[test]
fn populate_hijacked_message_points_at_the_provisioner() {
    let msg = populate_hijacked_message("plex-config", Some("pv-empty"));
    assert!(msg.contains("`plex-config`"), "{msg}");
    assert!(msg.contains("pv-empty"), "{msg}");
    assert!(msg.contains("AnyVolumeDataSource"), "{msg}");
    assert!(msg.contains("terminal"), "{msg}");
    assert!(
        populate_hijacked_message("plex-config", None).contains("another PersistentVolume"),
        "an unnamed volume must not render as an empty backtick pair"
    );
}

/// A populator that no-op'd long ago and is then asked to populate a FRESHLY re-created
/// claim must measure its `waitTimeout` from the re-open, not from its own creation —
/// otherwise the window is already spent, and a `fromPolicy` source (which defaults to
/// `Continue`) skips the wait and provisions an EMPTY volume the instant the snapshot
/// happens not to be there yet.
#[test]
fn wait_window_re_anchors_when_a_recreated_claim_reopens_resolution() {
    let with_ready = |reason: &str, at: &str| -> Restore {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Restore",
            "metadata": { "name": "r", "namespace": "ns", "generation": 1 },
            "spec": {
                "source": { "fromPolicy": { "name": "cfg" } },
                "target": { "populator": {} }
            },
            "status": { "conditions": [{
                "type": "Ready", "status": "False", "reason": reason, "message": "m",
                "lastTransitionTime": at
            }] }
        }))
        .expect("valid Restore")
    };

    // 2026-01-01T00:00:00Z == 1767225600. The Restore itself was created long before.
    let created = 1_000_000_000;
    let reopened = with_ready("ClaimRecreated", "2026-01-01T00:00:00Z");
    assert_eq!(wait_window_anchor(&reopened, created), 1_767_225_600);

    // Any other Ready reason leaves the window anchored at creation.
    let normal = with_ready("PopulatingPrimePvc", "2026-01-01T00:00:00Z");
    assert_eq!(wait_window_anchor(&normal, created), created);

    // A re-open that somehow predates creation never SHORTENS the window.
    let stale = with_ready("ClaimRecreated", "2001-09-09T01:46:40Z");
    assert_eq!(wait_window_anchor(&stale, created), created);

    // Net effect: the user's 5m window is fully available again from the re-open.
    assert_eq!(
        wait_remaining_secs(
            wait_window_anchor(&reopened, created),
            Some("5m"),
            1_767_225_660,
        ),
        Some(240),
        "the configured waitTimeout must actually apply to the re-created claim"
    );
}

// --- restore_flags (M2 flag sweep controller-glue guard) ---

#[test]
fn restore_flags_absent_options_map_to_all_none() {
    // No `spec.options` set → every knob defaults, reproducing today's argv.
    let flags = restore_flags(&None);
    assert_eq!(flags.ignore_permission_errors, None);
    assert_eq!(flags.write_files_atomically, None);
    assert_eq!(flags.parallel, None);
    assert_eq!(flags.write_sparse_files, None);
    assert_eq!(flags.skip_owners, None);
    assert_eq!(flags.skip_permissions, None);
    assert_eq!(flags.skip_times, None);
    assert_eq!(flags.overwrite_files, None);
    assert_eq!(flags.overwrite_directories, None);
    assert_eq!(flags.overwrite_symlinks, None);
    assert_eq!(flags.ignore_errors, None);
    assert_eq!(flags.skip_existing, None);
    assert!(!flags.delete_extra);
}

#[test]
fn restore_flags_maps_every_options_field() {
    use kopiur_api::restore::RestoreOptions;
    let flags = restore_flags(&Some(RestoreOptions {
        enable_file_deletion: false,
        ignore_permission_errors: Some(true),
        write_files_atomically: Some(false),
        parallel: Some(6),
        write_sparse_files: Some(true),
        skip_owners: Some(false),
        skip_permissions: Some(true),
        skip_times: Some(false),
        overwrite_files: Some(true),
        overwrite_directories: Some(false),
        overwrite_symlinks: Some(true),
        ignore_errors: Some(false),
        skip_existing: Some(true),
    }));
    assert_eq!(flags.ignore_permission_errors, Some(true));
    assert_eq!(flags.write_files_atomically, Some(false));
    assert_eq!(flags.parallel, Some(6));
    assert_eq!(flags.write_sparse_files, Some(true));
    assert_eq!(flags.skip_owners, Some(false));
    assert_eq!(flags.skip_permissions, Some(true));
    assert_eq!(flags.skip_times, Some(false));
    assert_eq!(flags.overwrite_files, Some(true));
    assert_eq!(flags.overwrite_directories, Some(false));
    assert_eq!(flags.overwrite_symlinks, Some(true));
    assert_eq!(flags.ignore_errors, Some(false));
    assert_eq!(flags.skip_existing, Some(true));
    assert!(!flags.delete_extra);
}

#[test]
fn restore_flags_enable_file_deletion_regression() {
    // THE regression test for the confirmed bug: `enableFileDeletion: true` was
    // documented as "exact mirror" deletion, settable via CRD/CLI/migrate, but
    // consumed by nothing — the controller only ever read
    // ignore_permission_errors/write_files_atomically. This must now map
    // through to `delete_extra`, which `RestoreOp::restore_options()` turns
    // into `Some(true)` and `restore_args` turns into `--delete-extra`.
    use kopiur_api::restore::RestoreOptions;
    let flags = restore_flags(&Some(RestoreOptions {
        enable_file_deletion: true,
        ..Default::default()
    }));
    assert!(
        flags.delete_extra,
        "enableFileDeletion: true must set delete_extra on the mover work-spec"
    );

    // End-to-end through the mover's RestoreOp -> kopia client RestoreOptions ->
    // argv, proving the whole chain (not just this one hop).
    let op = RestoreOp {
        source: RestoreSelection::Snapshot("s".into()),
        target_path: "/data".into(),
        anchor: Default::default(),
        ignore_permission_errors: flags.ignore_permission_errors,
        write_files_atomically: flags.write_files_atomically,
        parallel: flags.parallel,
        write_sparse_files: flags.write_sparse_files,
        skip_owners: flags.skip_owners,
        skip_permissions: flags.skip_permissions,
        skip_times: flags.skip_times,
        overwrite_files: flags.overwrite_files,
        overwrite_directories: flags.overwrite_directories,
        overwrite_symlinks: flags.overwrite_symlinks,
        ignore_errors: flags.ignore_errors,
        skip_existing: flags.skip_existing,
        delete_extra: flags.delete_extra,
    };
    assert_eq!(op.restore_options().delete_extra, Some(true));
}

// --- §3.6 CR-catalog search: select_recorded_source ---------------------------

/// Build a Snapshot CR row for the selector: name, identity triple, optional
/// kopia id / endTime / recorded uid. `recorded: None` models a pre-feature row.
fn catalog_row(
    name: &str,
    username: &str,
    hostname: &str,
    source_path: Option<&str>,
    kopia_id: &str,
    end_time: Option<&str>,
    recorded_uid: Option<Option<i64>>,
) -> Snapshot {
    let mut status = serde_json::json!({
        "snapshot": {
            "kopiaSnapshotID": kopia_id,
            "identity": { "username": username, "hostname": hostname },
        },
    });
    if let Some(p) = source_path {
        status["snapshot"]["identity"]["sourcePath"] = serde_json::json!(p);
    }
    if let Some(t) = end_time {
        status["timing"] = serde_json::json!({ "endTime": t });
    }
    if let Some(uid) = recorded_uid {
        let mut rec = serde_json::json!({ "schema": 1, "src": "explicit" });
        if let Some(u) = uid {
            rec["uid"] = serde_json::json!(u);
        }
        status["recorded"] = rec;
    }
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": name, "namespace": "app" },
        "spec": {},
        "status": status,
    }))
    .expect("catalog row fixture")
}

fn triple(username: &str, hostname: &str, source_path: Option<&str>) -> ResolvedIdentity {
    ResolvedIdentity {
        username: username.into(),
        hostname: hostname.into(),
        source_path: source_path.map(String::from),
    }
}

use kopiur_api::common::ResolvedIdentity;

fn cutoff(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s)
        .unwrap()
        .with_timezone(&chrono::Utc)
}

#[test]
fn select_recorded_source_picks_the_newest_matching_row() {
    let rows = vec![
        catalog_row(
            "old",
            "pg",
            "app",
            Some("/data"),
            "k1",
            Some("2026-06-01T00:00:00Z"),
            Some(Some(3001)),
        ),
        catalog_row(
            "new",
            "pg",
            "app",
            Some("/data"),
            "k3",
            Some("2026-06-03T00:00:00Z"),
            Some(Some(3003)),
        ),
        catalog_row(
            "mid",
            "pg",
            "app",
            Some("/data"),
            "k2",
            Some("2026-06-02T00:00:00Z"),
            Some(Some(3002)),
        ),
    ];
    let row = select_recorded_source(&triple("pg", "app", Some("/data")), None, 0, None, &rows)
        .expect("a match");
    assert_eq!(row.name, "new");
    assert_eq!(row.meta.uid, Some(3003));
    // COHERENCE (the P1 rule): the selected row carries ITS OWN kopia id, so the
    // caller pins the restored data to the same snapshot the identity came from.
    assert_eq!(row.kopia_snapshot_id, "k3");
    assert_eq!(row.identity.username, "pg");
}

#[test]
fn select_recorded_source_matches_identity_exactly_and_source_path_any_when_absent() {
    let rows = vec![
        catalog_row(
            "other-user",
            "redis",
            "app",
            Some("/data"),
            "k1",
            Some("2026-06-03T00:00:00Z"),
            Some(Some(1)),
        ),
        catalog_row(
            "other-host",
            "pg",
            "media",
            Some("/data"),
            "k2",
            Some("2026-06-03T00:00:00Z"),
            Some(Some(2)),
        ),
        catalog_row(
            "other-path",
            "pg",
            "app",
            Some("/other"),
            "k3",
            Some("2026-06-02T00:00:00Z"),
            Some(Some(3)),
        ),
        catalog_row(
            "match",
            "pg",
            "app",
            Some("/data"),
            "k4",
            Some("2026-06-01T00:00:00Z"),
            Some(Some(4)),
        ),
    ];
    // An explicit sourcePath in the triple must match exactly.
    let row = select_recorded_source(&triple("pg", "app", Some("/data")), None, 0, None, &rows)
        .expect("a match");
    assert_eq!(row.name, "match");
    // An absent sourcePath matches any path for the identity (the mover's own
    // selector semantics) — newest of /other (June 2) vs /data (June 1) wins.
    let row =
        select_recorded_source(&triple("pg", "app", None), None, 0, None, &rows).expect("a match");
    assert_eq!(row.name, "other-path");
    // No identity match at all -> None (the MissingRecordedIdentity hold).
    assert!(select_recorded_source(&triple("nope", "app", None), None, 0, None, &rows).is_none());
}

#[test]
fn select_recorded_source_requires_status_recorded() {
    // The newest row matches the identity but predates the kopiur-meta feature
    // (no status.recorded): it must be SKIPPED, not returned meta-less.
    let rows = vec![
        catalog_row(
            "bare-newest",
            "pg",
            "app",
            Some("/data"),
            "k2",
            Some("2026-06-03T00:00:00Z"),
            None,
        ),
        catalog_row(
            "recorded-older",
            "pg",
            "app",
            Some("/data"),
            "k1",
            Some("2026-06-01T00:00:00Z"),
            Some(Some(3001)),
        ),
    ];
    let row = select_recorded_source(&triple("pg", "app", Some("/data")), None, 0, None, &rows)
        .expect("the recorded row");
    assert_eq!(row.name, "recorded-older");
    assert_eq!(row.meta.uid, Some(3001));
    assert_eq!(
        row.kopia_snapshot_id, "k1",
        "the id pinned as data must be the recorded row's own"
    );
    // Only bare rows -> None.
    let bare = vec![catalog_row(
        "bare",
        "pg",
        "app",
        Some("/data"),
        "k2",
        Some("2026-06-03T00:00:00Z"),
        None,
    )];
    assert!(
        select_recorded_source(&triple("pg", "app", Some("/data")), None, 0, None, &bare).is_none()
    );
}

#[test]
fn select_recorded_source_pins_by_snapshot_id() {
    let rows = vec![
        catalog_row(
            "new",
            "pg",
            "app",
            Some("/data"),
            "k3",
            Some("2026-06-03T00:00:00Z"),
            Some(Some(3003)),
        ),
        catalog_row(
            "old",
            "pg",
            "app",
            Some("/data"),
            "k1",
            Some("2026-06-01T00:00:00Z"),
            Some(Some(3001)),
        ),
    ];
    // The pin wins over "newest".
    let row = select_recorded_source(
        &triple("pg", "app", Some("/data")),
        None,
        0,
        Some("k1"),
        &rows,
    )
    .expect("the pinned row");
    assert_eq!(row.name, "old");
    assert_eq!(row.meta.uid, Some(3001));
    assert_eq!(row.kopia_snapshot_id, "k1", "pinned id round-trips");
    // A pin that matches no row -> None.
    assert!(
        select_recorded_source(
            &triple("pg", "app", Some("/data")),
            None,
            0,
            Some("kX"),
            &rows
        )
        .is_none()
    );
    // A pinned row must still match the identity triple (a foreign row with the
    // same manifest id in the namespace cannot be picked up).
    assert!(
        select_recorded_source(&triple("redis", "app", None), None, 0, Some("k1"), &rows).is_none()
    );
}

#[test]
fn select_recorded_source_honors_as_of_and_offset_like_the_mover_selection() {
    let rows = vec![
        catalog_row(
            "k3",
            "pg",
            "app",
            Some("/data"),
            "k3",
            Some("2026-06-03T00:00:00Z"),
            Some(Some(3)),
        ),
        catalog_row(
            "k2",
            "pg",
            "app",
            Some("/data"),
            "k2",
            Some("2026-06-02T00:00:00Z"),
            Some(Some(2)),
        ),
        catalog_row(
            "k1",
            "pg",
            "app",
            Some("/data"),
            "k1",
            Some("2026-06-01T00:00:00Z"),
            Some(Some(1)),
        ),
    ];
    let t = triple("pg", "app", Some("/data"));
    // asOf keeps rows at-or-before the cutoff (mirrors filter_as_of).
    let row =
        select_recorded_source(&t, Some(cutoff("2026-06-02T12:00:00Z")), 0, None, &rows).unwrap();
    assert_eq!(row.name, "k2");
    // Exactly AT an endTime keeps it.
    let row =
        select_recorded_source(&t, Some(cutoff("2026-06-02T00:00:00Z")), 0, None, &rows).unwrap();
    assert_eq!(row.name, "k2");
    // asOf composes with offset ("the previous one as of just after k2").
    let row =
        select_recorded_source(&t, Some(cutoff("2026-06-02T12:00:00Z")), 1, None, &rows).unwrap();
    assert_eq!(row.name, "k1");
    // Before everything -> None.
    assert!(
        select_recorded_source(&t, Some(cutoff("2026-05-01T00:00:00Z")), 0, None, &rows).is_none()
    );
    // offset semantics mirror pick_offset: out-of-range None, negative clamps.
    let row = select_recorded_source(&t, None, 2, None, &rows).unwrap();
    assert_eq!(row.name, "k1");
    assert!(select_recorded_source(&t, None, 3, None, &rows).is_none());
    let row = select_recorded_source(&t, None, -1, None, &rows).unwrap();
    assert_eq!(row.name, "k3");
}

#[test]
fn select_recorded_source_undated_rows_sort_last_and_are_excluded_under_a_cutoff() {
    let rows = vec![
        catalog_row(
            "undated",
            "pg",
            "app",
            Some("/data"),
            "kU",
            None,
            Some(Some(9)),
        ),
        catalog_row(
            "dated",
            "pg",
            "app",
            Some("/data"),
            "k1",
            Some("2026-06-01T00:00:00Z"),
            Some(Some(1)),
        ),
    ];
    let t = triple("pg", "app", Some("/data"));
    // Newest-first puts the dated row first; the undated one is still reachable.
    let row = select_recorded_source(&t, None, 0, None, &rows).unwrap();
    assert_eq!(row.name, "dated");
    let row = select_recorded_source(&t, None, 1, None, &rows).unwrap();
    assert_eq!(row.name, "undated");
    // Under a cutoff an undated row cannot prove membership -> excluded.
    let got = select_recorded_source(&t, Some(cutoff("2026-06-02T00:00:00Z")), 1, None, &rows);
    assert!(
        got.is_none(),
        "undated row must be excluded under asOf, got {got:?}"
    );
}

#[test]
fn snapshot_inherit_active_matches_only_the_snapshot_variant() {
    use kopiur_api::common::{InheritSecurityContextFrom, MoverSpec, SnapshotInherit};
    let with_inherit = |i: Option<InheritSecurityContextFrom>| -> Restore {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Restore",
            "metadata": { "name": "r", "namespace": "app" },
            "spec": {
                "source": { "snapshotRef": { "name": "s" } },
                "target": { "pvcRef": { "name": "dst" } },
                "mover": serde_json::to_value(MoverSpec {
                    inherit_security_context_from: i,
                    ..Default::default()
                }).unwrap(),
            }
        }))
        .expect("restore fixture")
    };
    // The gate for the P1 coherence rule: only the `snapshot` variant makes
    // `resolve_snapshot` pin data + identity from one CR-catalog row.
    assert!(snapshot_inherit_active(&with_inherit(Some(
        InheritSecurityContextFrom::Snapshot(SnapshotInherit {})
    ))));
    assert!(!snapshot_inherit_active(&with_inherit(None)));
    assert!(!snapshot_inherit_active(&with_inherit(Some(
        InheritSecurityContextFrom::WorkloadSelector(kopiur_api::PodSelector {
            pod_selector: Default::default(),
            container: None,
        })
    ))));
}

// --- recorded_inherit_verdict: the SecurityContextInherited text for snapshot inherit ---

use kopiur_api::recorded::RecordedSrc;

#[test]
fn recorded_verdict_uid_pinned_is_true_and_names_snapshot_uid_and_provenance() {
    let v = recorded_inherit_verdict(
        "app/pg-b1",
        Some(3001),
        RecordedSrc::Inherited,
        Some(3001),
        Some(65532),
    );
    assert!(v.ok);
    assert_eq!(v.reason, RECORDED_APPLIED_REASON);
    assert!(v.message.contains("app/pg-b1"), "{}", v.message);
    assert!(v.message.contains("uid 3001"), "{}", v.message);
    assert!(v.message.contains("`inherited`"), "{}", v.message);
    // Only src: inherited may claim the identity tracked the workload.
    assert!(v.message.contains("live workload"), "{}", v.message);
}

#[test]
fn recorded_verdict_explicit_provenance_never_claims_workload_tracking() {
    let v = recorded_inherit_verdict("app/pg-b1", Some(3001), RecordedSrc::Explicit, None, None);
    assert!(v.ok);
    assert!(v.message.contains("`explicit`"), "{}", v.message);
    assert!(
        v.message.contains("never workload-derived"),
        "{}",
        v.message
    );
    assert!(
        !v.message.contains("identity the workload actually ran as"),
        "explicit provenance must not claim workload tracking: {}",
        v.message
    );

    let d = recorded_inherit_verdict("app/pg-b1", Some(3001), RecordedSrc::Defaults, None, None);
    assert!(d.message.contains("not from the workload"), "{}", d.message);
    let u = recorded_inherit_verdict("app/pg-b1", Some(3001), RecordedSrc::Unknown, None, None);
    assert!(u.message.contains("does not recognize"), "{}", u.message);
}

#[test]
fn recorded_verdict_baseline_only_meta_warns_recorded_pinned_no_uid() {
    use kopiur_api::common::MOVER_NONROOT_ID;
    // Nothing beyond the hardened baseline: no uid, no gid, fsGroup absent or the
    // hardened 65532 -> a no-op inherit must NOT report a positive condition.
    for fs in [None, Some(MOVER_NONROOT_ID)] {
        let v = recorded_inherit_verdict("app/pg-b1", None, RecordedSrc::Defaults, None, fs);
        assert!(!v.ok, "fsGroup {fs:?} is baseline");
        assert_eq!(v.reason, RECORDED_PINNED_NO_UID_REASON);
        assert!(v.message.contains("app/pg-b1"), "{}", v.message);
        assert!(v.message.contains("65532"), "{}", v.message);
        assert!(v.message.contains("runAsUser"), "fix named: {}", v.message);
    }
}

#[test]
fn recorded_verdict_non_baseline_group_only_is_true() {
    // fsGroup-only beyond the baseline: the blessed FsGroupMatch restore shape.
    let v = recorded_inherit_verdict("app/pg-b1", None, RecordedSrc::Inherited, None, Some(2000));
    assert!(v.ok, "{}", v.message);
    assert_eq!(v.reason, RECORDED_APPLIED_REASON);
    assert!(v.message.contains("fsGroup 2000"), "{}", v.message);
    // gid-only is a real contribution too (group-readable data).
    let g = recorded_inherit_verdict("app/pg-b1", None, RecordedSrc::Explicit, Some(1000), None);
    assert!(g.ok, "{}", g.message);
    assert!(g.message.contains("gid 1000"), "{}", g.message);
}

#[test]
fn recorded_verdict_root_uid_makes_the_elevation_visible() {
    // §3.4: a recorded uid 0 must be auditable from the condition text alone —
    // name ROOT, the snapshot it came from, and the forgeability of the record.
    let v = recorded_inherit_verdict("app/pg-b1", Some(0), RecordedSrc::Explicit, Some(0), None);
    assert!(v.ok, "{}", v.message);
    assert!(v.message.contains("ROOT (uid 0)"), "{}", v.message);
    assert!(v.message.contains("app/pg-b1"), "{}", v.message);
    assert!(v.message.contains("forge"), "{}", v.message);
    assert!(v.message.contains("privileged-movers"), "{}", v.message);
}
