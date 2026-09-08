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
        source_path: None,
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
fn wait_remaining_counts_down_from_the_anchor_and_closes() {
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
    assert!(restore_awaiting_launch(Some(&Pending)));
    assert!(restore_awaiting_launch(Some(&Resolving)));
    // A live (or just-terminal) mover Job must be observed, never re-gated —
    // and a populator's non-terminal `Completed` heartbeat must not be flipped
    // back to `Pending`.
    assert!(!restore_awaiting_launch(Some(&Restoring)));
    assert!(!restore_awaiting_launch(Some(&Completed)));
    assert!(!restore_awaiting_launch(Some(&Failed)));
    // A phase written by a NEWER operator: never re-gate it back to `Pending`,
    // which would fight a mover the newer operator may already have launched.
    assert!(!restore_awaiting_launch(Some(&RestorePhase::Unknown(
        "Staging".into()
    ))));
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

// --- #393: the readiness gate's tri-state repository lookup ---------------

/// A `Snapshot` fixture, parsed the cluster's way. `pin` is the
/// `spec.repository` mint-time pin (`None` ⇒ nothing derivable at all).
fn snapshot_fixture(pin: Option<&str>) -> kopiur_api::Snapshot {
    let mut spec = serde_json::json!({ "sources": [ { "pvc": { "name": "data" } } ] });
    if let Some(name) = pin {
        spec["repository"] = serde_json::json!({ "kind": "Repository", "name": name });
    }
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": "b", "namespace": "apps" },
        "spec": spec,
    }))
    .expect("Snapshot fixture")
}

/// A `SnapshotPolicy` fixture: one repository (`repository`) or a
/// multi-repository fan-out (`repositories`), which has no single ref.
fn policy_fixture(repositories: &[&str]) -> kopiur_api::SnapshotPolicy {
    let mut spec = serde_json::json!({ "sources": [ { "pvc": { "name": "data" } } ] });
    match repositories {
        [one] => spec["repository"] = serde_json::json!({ "kind": "Repository", "name": one }),
        many => {
            spec["repositories"] = serde_json::json!(
                many.iter()
                    .map(|n| serde_json::json!({ "kind": "Repository", "name": n }))
                    .collect::<Vec<_>>()
            )
        }
    }
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "cfg", "namespace": "apps" },
        "spec": spec,
    }))
    .expect("SnapshotPolicy fixture")
}

/// The distinction the pre-#393 chained `get_opt(..).and_then(..)` erased: a
/// `Snapshot` ROW that does not exist is a supported, waited-for shape; a row
/// that exists but names no repository is a spec problem.
#[test]
fn snapshot_lookup_separates_a_missing_row_from_an_underivable_one() {
    // Missing row: the shape the `waitTimeout` window is FOR. The gate must not
    // engage, or `onMissingSnapshot: Fail` could never fire for a typo'd ref.
    assert_eq!(
        classify_snapshot_lookup(None, "apps"),
        RepoRefLookup::SnapshotRowMissing
    );
    // Row present WITH a pin: derived, relative to the snapshot's namespace.
    let pinned = snapshot_fixture(Some("nas"));
    match classify_snapshot_lookup(Some(&pinned), "apps") {
        RepoRefLookup::Derived(rref, ns) => {
            assert_eq!(rref.name, "nas");
            assert_eq!(ns, "apps");
        }
        other => panic!("a pinned Snapshot must derive its repository: {other:?}"),
    }
    // Row present WITHOUT a pin (no status, no spec.repository, no repository
    // owner): the object is there, so nothing will appear later to fix it —
    // falls through to downstream validation rather than parking forever.
    assert_eq!(
        classify_snapshot_lookup(Some(&snapshot_fixture(None)), "apps"),
        RepoRefLookup::NotDerivable
    );
}

/// The other half: a `SnapshotPolicy` that does not exist parks the gate; one
/// that exists but fans out over several repositories does not (the gate must
/// never guess repository #1).
#[test]
fn policy_lookup_separates_a_missing_policy_from_a_multi_repo_one() {
    assert_eq!(
        classify_policy_lookup(None, "apps", "cfg"),
        RepoRefLookup::ReferentMissing {
            kind: "SnapshotPolicy",
            namespace: Some("apps".into()),
            name: "cfg".into(),
        }
    );
    let single = policy_fixture(&["nas"]);
    match classify_policy_lookup(Some(&single), "apps", "cfg") {
        RepoRefLookup::Derived(rref, ns) => {
            assert_eq!(rref.name, "nas");
            assert_eq!(ns, "apps");
        }
        other => panic!("a single-repository policy must derive its repository: {other:?}"),
    }
    // Multi-repo with no explicit selection: `resolve_restore_repository` fails
    // closed downstream listing the valid choices — parking would hide that.
    assert_eq!(
        classify_policy_lookup(Some(&policy_fixture(&["a", "b"])), "apps", "cfg"),
        RepoRefLookup::NotDerivable
    );
}

#[test]
fn referent_missing_message_says_what_why_and_how() {
    let msg = referent_missing_restore_message("SnapshotPolicy", Some("apps"), "cfg");
    // WHAT is missing, namespaced.
    assert!(msg.contains("SnapshotPolicy `apps/cfg`"), "{msg}");
    // WHY it blocks: the repository is derived from it and cannot be verified.
    assert!(msg.contains("derived from it"), "{msg}");
    // The #393 promise itself: the window is NOT running meanwhile.
    assert!(msg.contains("waitTimeout"), "{msg}");
    assert!(msg.contains("status.waitStartedAt"), "{msg}");
    // HOW to clear it.
    assert!(msg.contains("Create the SnapshotPolicy"), "{msg}");
    // A cluster-scoped referent must not be given an invented namespace.
    let cluster = referent_missing_restore_message("ClusterRepository", None, "offsite");
    assert!(cluster.contains("ClusterRepository `offsite`"), "{cluster}");
    assert!(!cluster.contains('/'), "{cluster}");
    // The reason is distinct from the not-Ready one: a SnapshotPolicy that was
    // never applied is not an unreachable backend.
    assert_ne!(
        crate::consts::RESTORE_REFERENT_MISSING_REASON,
        crate::consts::REPOSITORY_NOT_READY_REASON
    );
    assert_eq!(
        crate::consts::RESTORE_REFERENT_MISSING_REASON,
        "RestoreReferentMissing"
    );
}

/// The park's gate condition must not outlive the park: the registry row is
/// age-independent, so a stale `ReferentAvailable=False` would keep `kubectl
/// kopiur doctor` reporting a restore that proceeded hours ago as blocked.
#[test]
fn the_referent_gate_condition_clears_only_when_it_is_stale() {
    use crate::consts::RESTORE_REFERENT_AVAILABLE_CONDITION;
    // Never parked: nothing to clear, and the healthy wire must not GROW the
    // condition (a write per pass would be pure churn).
    assert!(cleared_referent_conditions(&restore_with_condition("Resolved", "True")).is_none());
    // Parked: flipped back to True in place, keeping the array a single row.
    let parked = restore_with_condition(RESTORE_REFERENT_AVAILABLE_CONDITION, "False");
    let cleared = cleared_referent_conditions(&parked).expect("a stale park must clear");
    let row = cleared
        .iter()
        .find(|c| c.type_ == RESTORE_REFERENT_AVAILABLE_CONDITION)
        .expect("the condition survives, flipped");
    assert_eq!(row.status, "True");
    assert_eq!(row.reason, crate::consts::RESTORE_REFERENT_FOUND_REASON);
    // Already cleared: idempotent, so the clear cannot flip-flop with the
    // condition writers that rebuild from the reconcile-start copy.
    let healthy = restore_with_condition(RESTORE_REFERENT_AVAILABLE_CONDITION, "True");
    assert!(cleared_referent_conditions(&healthy).is_none());
}

/// The write-loop guard: a pass that clears the referent gate must carry the
/// cleared conditions forward, or the UNCONDITIONAL gate parks downstream
/// (`run_restore_mover`'s `MissingCaBundle`/`MissingServiceAccount`/
/// `PrivilegedMover`/`MissingCredentials` writes) rebuild the array from the
/// reconcile-start copy and put `ReferentAvailable=False` straight back.
///
/// That alternation is not cosmetic: both writes bump `resourceVersion`, each
/// wakes the watch and re-enqueues immediately, so the pair repeats forever (two
/// writes + an Event per iteration) for as long as the Secret/SA/ConfigMap is
/// missing — precisely the GitOps bring-up this feature serves. Those parks are
/// byte-identical no-ops today only because nothing writes conditions ahead of
/// them; this test pins the property that keeps that true.
///
/// The CALLER half of the contract is enforced by the compiler rather than here:
/// `RepositoryGate::Proceed` carries the `CarriedRestore` to continue with, so
/// `reconcile_inner` cannot get a `&Restore` for the rest of the pass without
/// taking the carried one. This test pins the mechanism that carrying provides.
#[test]
fn a_cleared_referent_condition_survives_a_downstream_gate_park() {
    use crate::consts::RESTORE_REFERENT_AVAILABLE_CONDITION;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
    let status_of = |conds: &[Condition]| {
        conds
            .iter()
            .find(|c| c.type_ == RESTORE_REFERENT_AVAILABLE_CONDITION)
            .map(|c| c.status.clone())
    };

    let parked = restore_with_condition(RESTORE_REFERENT_AVAILABLE_CONDITION, "False");
    let cleared = cleared_referent_conditions(&parked).expect("a stale park clears");
    // Built through the one construction site the gate uses, so this exercises
    // the real seam rather than a re-implementation of it.
    let carrier = carried_after_clear(&parked, Some(&cleared));
    assert!(
        matches!(carrier, CarriedRestore::Cleared(_)),
        "a cleared pass must carry an OWNED copy, never the stale borrow"
    );
    let carried = carrier.get().clone();

    // What a downstream gate park computes from the CARRIED copy: the clear holds.
    let after_park = io::upsert_gate(
        &existing_conditions(&carried),
        &kopiur_api::gates::MISSING_CREDENTIALS_GATE,
        "the credentials Secret is not in the mover namespace",
        carried.metadata.generation,
    );
    assert_eq!(
        status_of(&after_park).as_deref(),
        Some("True"),
        "carrying the cleared conditions forward must survive an unconditional \
         downstream gate park: {after_park:?}"
    );
    // ...and the same park computed from the reconcile-start copy is the write
    // that used to alternate with the clear. Asserted so this test fails loudly
    // if the clobber ever stops being reproducible (i.e. the guard goes vacuous).
    let clobbered = io::upsert_gate(
        &existing_conditions(&parked),
        &kopiur_api::gates::MISSING_CREDENTIALS_GATE,
        "the credentials Secret is not in the mover namespace",
        parked.metadata.generation,
    );
    assert_eq!(
        status_of(&clobbered).as_deref(),
        Some("False"),
        "the reconcile-start copy still carries the stale park — that is the write \
         the carried copy exists to prevent"
    );

    // The carried copy is also a fixed point: a second pass over it clears
    // nothing, so the loop cannot restart from the other side.
    assert!(cleared_referent_conditions(&carried).is_none());
    // Carrying preserves everything else about the object (only conditions move).
    assert_eq!(carried.metadata.generation, parked.metadata.generation);
    assert_eq!(carried.spec, parked.spec);

    // Both arms of the construction site: nothing cleared ⇒ the original is
    // BORROWED (the common path pays no clone and changes nothing).
    let untouched = restore_with_condition("Resolved", "True");
    let same = carried_after_clear(
        &untouched,
        cleared_referent_conditions(&untouched).as_deref(),
    );
    assert!(
        matches!(same, CarriedRestore::Unchanged(_)),
        "the common path must borrow: no clone, no allocation, nothing changed"
    );
    assert_eq!(same.get().status, untouched.status);
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
fn a_populator_is_terminal_at_the_guard_on_neither_completed_nor_failed() {
    use PopulatorState::{AwaitingClaim, DirectTarget};
    use RestorePhase::{Completed, Failed, Pending, Resolving, Restoring};

    // A populator `Completed` (mover done with the prime PVC, rebind still pending)
    // must NOT be terminal at the guard, or the rebind never runs. This non-terminal
    // `Completed` is also what makes a populator `Restore` REUSABLE: delete the claiming
    // PVC and apply a fresh one with the same `dataSourceRef` and reconcile falls through
    // here to populate the new (unbound) claim, rather than short-circuiting as "consumed".
    assert!(!phase_is_terminal_at_guard(&Completed, AwaitingClaim));
    // A direct restore writes the target itself, so `Completed` IS terminal.
    assert!(phase_is_terminal_at_guard(&Completed, DirectTarget));

    // #443: a populator `Failed` is the AGGREGATE over its claims, so it must not
    // short-circuit either — one failed claim would otherwise freeze every sibling.
    // Terminality moved to the CLAIM (`claim_drive` ⇒ Settled), and the prime of a
    // hijacked populate is protected by `claim_artifacts_reapable` instead of by
    // this guard. The two assertions below and that predicate are ONE change.
    assert!(!phase_is_terminal_at_guard(&Failed, AwaitingClaim));
    assert!(
        !ClaimReason::PopulateHijacked.artifacts_reapable(),
        "the guard relaxation above is only safe while the reaper refuses a hijacked \
         populate's artifacts — these must never be changed apart"
    );
    assert!(
        claim_drive(
            Some(&kopiur_api::RestoreClaimStatus {
                uid: Some("u1".into()),
                phase: Some(kopiur_api::RestoreClaimPhase::Failed),
                reason: Some(crate::consts::POPULATE_HIJACKED_REASON.into()),
                ..Default::default()
            }),
            "u1",
        ) == ClaimDrive::Settled,
        "a hijacked populate's claim must rest, not be re-driven every 120s"
    );

    // A DIRECT restore is one-shot: `Failed` stays terminal, and a retry is a NEW
    // Restore.
    assert!(phase_is_terminal_at_guard(&Failed, DirectTarget));

    // In-flight phases are never terminal.
    for p in [
        Pending,
        Resolving,
        Restoring,
        // An uninterpretable phase must not short-circuit the reconcile into
        // "nothing left to do".
        RestorePhase::Unknown("Staging".into()),
    ] {
        assert!(!phase_is_terminal_at_guard(&p, AwaitingClaim));
        assert!(!phase_is_terminal_at_guard(&p, DirectTarget));
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
    // A pinned `NoSnapshot` is always the deploy-or-restore Empty decision — even
    // if a kopiaSnapshotID somehow co-exists, NoSnapshot wins (data-safety: a later
    // snapshot must never retarget a volume that already came up empty).
    assert_eq!(
        pinned_decision(Some(&resolved_with(
            Some(ResolutionOutcome::NoSnapshot),
            None
        ))),
        Some(Resolution::Empty)
    );

    // A pinned snapshot id resolves to that id (with the explicit Snapshot outcome…).
    assert_eq!(
        pinned_decision(Some(&resolved_with(
            Some(ResolutionOutcome::Snapshot),
            Some("k7")
        ))),
        Some(Resolution::Snapshot("k7".into()))
    );
    // …and a LEGACY pin (id present, `resolution` field absent) reads the same,
    // so an in-flight restore pinned before this field existed keeps its target.
    assert_eq!(
        pinned_decision(Some(&resolved_with(None, Some("k7")))),
        Some(Resolution::Snapshot("k7".into()))
    );

    // A fresh, un-pinned restore must resolve.
    assert_eq!(pinned_decision(None), None);
}

/// #443: everything `pinned_decision` used to do for a POPULATOR now happens per
/// claim, keyed on that claim's own pin and phase. The two arms that only ever
/// existed for a populator — the pre-fix `Completed`-but-unpinned back-fill and
/// the #233 already-bound carve-out from it — must still be exactly as careful,
/// or a re-created claim comes up EMPTY instead of restoring.
#[test]
fn the_populator_arms_of_pinned_decision_moved_intact_onto_the_claim() {
    use kopiur_api::{RestoreClaimPhase as P, RestoreClaimStatus};

    let record = |phase: Option<P>, resolved: Option<ResolvedRestore>| RestoreClaimStatus {
        uid: Some("u1".into()),
        phase,
        resolved,
        ..Default::default()
    };

    // The pre-fix stuck populator: the claim reads `Populated` with NOTHING pinned.
    // A snapshot-resolved claim ALWAYS pins before it can be Populated, so this
    // unambiguously means the decision was "empty" — back-fill, do NOT re-resolve.
    assert_eq!(
        claim_pinned_decision(&record(Some(P::Populated), None)),
        Some(Resolution::Empty)
    );
    // The #233 carve-out: an already-bound no-op on a DEFERRED source never ran the
    // mover, and the mover is what pins a deferred source. Back-filling `NoSnapshot`
    // here would durably record "this restore decided to come up empty".
    assert_eq!(
        claim_pinned_decision(&record(Some(P::AlreadyBound), None)),
        None
    );
    // A real pin is honored either way, so a re-created claim restores the SAME
    // snapshot (ADR §4.6: pinned once, never re-resolved).
    assert_eq!(
        claim_pinned_decision(&record(
            Some(P::AlreadyBound),
            Some(resolved_with(Some(ResolutionOutcome::NoSnapshot), None))
        )),
        Some(Resolution::Empty)
    );
    assert_eq!(
        claim_pinned_decision(&record(
            Some(P::AlreadyBound),
            Some(resolved_with(Some(ResolutionOutcome::Snapshot), Some("k9")))
        )),
        Some(Resolution::Snapshot("k9".into()))
    );
    // A fresh claim resolves.
    assert_eq!(claim_pinned_decision(&record(None, None)), None);
}

/// A claim record in `phase`/`reason`, for the pure tables below.
fn claim_record(
    phase: Option<kopiur_api::RestoreClaimPhase>,
    reason: Option<&str>,
) -> kopiur_api::RestoreClaimStatus {
    kopiur_api::RestoreClaimStatus {
        uid: Some("u1".into()),
        phase,
        reason: reason.map(str::to_string),
        ..Default::default()
    }
}

/// #443 review item 2 — the steady Settled sweep exists for ONE shape: a
/// finished claim whose finalize crashed and left an orphan prime. It must NOT
/// touch a `Failed` claim's mover Job or prime: the record tells the user to
/// read that Job's pod logs, restore movers carry no TTL, and pre-#443 both
/// survived until the user acted (a `Failed` populator was terminal at the
/// guard). Sweeping them 120 s later deletes the evidence the message points at.
#[test]
fn a_settled_failed_claim_keeps_its_job_and_prime_for_the_user_to_read() {
    use crate::consts::{
        MOVER_JOB_FAILED_REASON, MOVER_POD_WEDGED_REASON, POPULATE_HIJACKED_REASON,
        RESTORE_POPULATED_REASON, RESTORE_TARGET_ALREADY_BOUND_REASON,
    };
    use kopiur_api::RestoreClaimPhase as P;

    // The crash-recovery shape the sweep is FOR.
    assert!(settled_artifacts_reapable(&claim_record(
        Some(P::Populated),
        Some(RESTORE_POPULATED_REASON)
    )));
    assert!(settled_artifacts_reapable(&claim_record(
        Some(P::AlreadyBound),
        Some(RESTORE_TARGET_ALREADY_BOUND_REASON)
    )));

    // Every failure keeps its artifacts until the claim is re-armed or dropped.
    for reason in [
        MOVER_JOB_FAILED_REASON,
        MOVER_POD_WEDGED_REASON,
        POPULATE_HIJACKED_REASON,
        crate::consts::SOURCE_PATH_AMBIGUOUS_REASON,
        crate::consts::RESTORE_SNAPSHOT_NOT_FOUND_REASON,
    ] {
        assert!(
            !settled_artifacts_reapable(&claim_record(Some(P::Failed), Some(reason))),
            "a Failed/{reason} claim must keep its Job and prime"
        );
    }

    // The hijack refusal still holds through the wider gate too — the two
    // predicates protect different things and neither may be dropped.
    assert!(!claim_artifacts_reapable(Some(POPULATE_HIJACKED_REASON)));

    // Unsettled phases cannot reach this arm; encoded as "leave it alone"
    // rather than a panic, so a hand-patched status cannot crash the loop.
    for phase in [
        None,
        Some(P::Pending),
        Some(P::Populating),
        Some(P::Rebinding),
    ] {
        assert!(!settled_artifacts_reapable(&claim_record(phase, None)));
    }
    assert!(!settled_artifacts_reapable(&claim_record(
        Some(P::Unknown("Staging".into())),
        None
    )));
}

/// #443 review item 1 — the zero-claimant pass must NULL the records of
/// claimants that are gone in the SAME patch that parks. Writing the park alone
/// left them standing, so the next pass 30 s later re-reaped them and
/// re-published `OrphanedPrimePvcReaped`, forever, for every app torn down with
/// its populator left in place.
#[test]
fn the_zero_claimant_park_nulls_the_records_of_gone_claimants() {
    let restore = restore_with_anchor(None);
    let (reason, message) = claims_summary(&ClaimsAggregate::NoClaims);

    let status = awaiting_claim_status(&restore, reason, &message, &["postgres-data".to_string()]);
    assert_eq!(
        status["claims"]["postgres-data"],
        serde_json::Value::Null,
        "the null must be EXPLICIT — an omitted key leaves the record standing: {status}"
    );
    // The pre-#443 zero-claim surface is unchanged.
    assert_eq!(status["phase"], "Pending");
    assert_eq!(status["target"]["pvcPrime"], "awaiting-claim");
    assert!(
        status["conditions"]
            .as_array()
            .expect("conditions array")
            .iter()
            .any(|c| c["type"] == "AwaitingClaim"
                && c["status"] == "True"
                && c["reason"] == crate::consts::AWAITING_PVC_DATA_SOURCE_REF_REASON),
        "{status}"
    );

    // Nothing gone ⇒ no `claims` key at all, so the steady zero-claim heartbeat
    // is a no-op under the deep merge predicate rather than a write every 30s.
    let quiet = awaiting_claim_status(&restore, reason, &message, &[]);
    assert!(quiet.get("claims").is_none(), "{quiet}");
    assert!(
        crate::io::status_merge_patch_is_noop(
            Some(&serde_json::json!({
                "phase": "Pending",
                "observedGeneration": 1,
                "conditions": quiet["conditions"].clone(),
                "target": { "pvcPrime": "awaiting-claim" },
            })),
            &quiet
        ),
        "an unchanged zero-claim park must not re-write: {quiet}"
    );
}

// --- the single-claim TOP-LEVEL mirror (#443 CI regression) -------------
//
// The overwhelmingly common populator is a one-PVC app, and for it the `Restore`
// simply IS that claim. `docs/restores.md` promises the deploy-or-restore
// decision is pinned to `status.resolved`, and `kubectl kopiur restore`/`status`
// read exactly that field — moving the pin under `status.claims.<pvc>.resolved`
// broke both, plus four e2e regressions.

/// A claims map holding exactly `records`, keyed in insertion order.
fn claims_of(
    records: &[(&str, kopiur_api::RestoreClaimStatus)],
) -> std::collections::BTreeMap<String, kopiur_api::RestoreClaimStatus> {
    records
        .iter()
        .map(|(name, record)| ((*name).to_string(), record.clone()))
        .collect()
}

/// One claim record, for the mirror tables.
fn mirror_record(
    phase: kopiur_api::RestoreClaimPhase,
    reason: &str,
    message: &str,
    resolved: Option<ResolutionOutcome>,
) -> kopiur_api::RestoreClaimStatus {
    kopiur_api::RestoreClaimStatus {
        uid: Some("u1".into()),
        phase: Some(phase),
        reason: Some(reason.to_string()),
        message: Some(message.to_string()),
        resolved: resolved.map(|o| resolved_with(Some(o), None)),
        ..Default::default()
    }
}

/// One condition off a status body.
fn condition_of(status: &serde_json::Value, type_: &str) -> Option<serde_json::Value> {
    status["conditions"]
        .as_array()?
        .iter()
        .find(|c| c["type"] == type_)
        .cloned()
}

/// The status body a fan-out pass writes for exactly these claims.
fn mirrored_status(
    claims: &std::collections::BTreeMap<String, kopiur_api::RestoreClaimStatus>,
) -> serde_json::Value {
    fanout_status(
        &restore_with_anchor(None),
        &std::collections::BTreeMap::new(),
        claims,
        &[],
    )
}

/// Deploy-or-restore, one claim: the pinned "no snapshot" decision must be
/// visible at `status.resolved` — a later snapshot must never be able to
/// retarget the volume — and the outcome must be SELF-DESCRIBING
/// (`Resolved=True/NoSnapshotContinue`) rather than looking like a restore that
/// wrote data.
#[test]
fn a_lone_deploy_or_restore_claim_pins_its_decision_to_the_top_level_status() {
    use crate::consts::NO_SNAPSHOT_CONTINUE_REASON;
    use kopiur_api::RestoreClaimPhase as P;

    let claims = claims_of(&[(
        "data",
        mirror_record(
            P::Populated,
            NO_SNAPSHOT_CONTINUE_REASON,
            "provisioned an empty volume",
            Some(ResolutionOutcome::NoSnapshot),
        ),
    )]);
    let status = mirrored_status(&claims);

    assert_eq!(
        status["resolved"]["resolution"],
        serde_json::json!("NoSnapshot"),
        "{status}"
    );
    assert_eq!(status["target"]["pvcRef"]["name"], "data", "{status}");
    let resolved = condition_of(&status, "Resolved").expect("a Resolved condition");
    assert_eq!(resolved["status"], "True", "{resolved}");
    assert_eq!(
        resolved["reason"], NO_SNAPSHOT_CONTINUE_REASON,
        "{resolved}"
    );
    let ready = condition_of(&status, "Ready").expect("a Ready condition");
    assert_eq!(ready["reason"], NO_SNAPSHOT_CONTINUE_REASON, "{ready}");
    assert_eq!(ready["message"], "provisioned an empty volume", "{ready}");
}

/// The #233 no-op, one claim: the completion must say WHY nothing happened, in
/// the claim's own words. The aggregate wording ("1/1 claims settled") does not
/// tell the user their live volume was left untouched, or how to actually
/// restore into it.
#[test]
fn a_lone_already_bound_claim_reports_its_own_reason_and_clears_the_mirror() {
    use crate::consts::RESTORE_TARGET_ALREADY_BOUND_REASON;
    use kopiur_api::RestoreClaimPhase as P;

    let claims = claims_of(&[(
        "data",
        mirror_record(
            P::AlreadyBound,
            RESTORE_TARGET_ALREADY_BOUND_REASON,
            &target_already_bound_message("data", Some("pv-1")),
            None,
        ),
    )]);
    let status = mirrored_status(&claims);

    assert_eq!(status["phase"], "Completed", "{status}");
    let ready = condition_of(&status, "Ready").expect("a Ready condition");
    assert_eq!(ready["status"], "True", "{ready}");
    assert_eq!(
        ready["reason"], RESTORE_TARGET_ALREADY_BOUND_REASON,
        "{ready}"
    );
    assert!(
        ready["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already bound"),
        "the mirrored message is the CLAIM's, verbatim: {ready}"
    );
    // Nothing pinned: the mirror leaves the top-level `resolved` UNTOUCHED
    // (wave 2, finding 5 — a `null` here erased an adopted legacy mover's pin
    // before `claim_finalize_pin` could fall back to it). The handshake is over,
    // so the reaped prime is no longer advertised.
    assert!(
        status.get("resolved").is_none(),
        "an unpinned single claim must not name `resolved` at all: {status}"
    );
    assert_eq!(
        status["target"]["pvcPrime"],
        serde_json::Value::Null,
        "{status}"
    );
    // A `Resolved` condition would be news about the SOURCE; an already-bound
    // no-op is news about the handshake, so none is written.
    assert!(condition_of(&status, "Resolved").is_none(), "{status}");
}

/// A real restore mirrors its snapshot pin; an in-flight one mirrors the prime
/// it is writing, so `status.target.pvcPrime` still names the volume to look at.
#[test]
fn a_lone_claim_mirrors_its_pin_and_its_prime_through_the_handshake() {
    use crate::consts::{POPULATING_PRIME_PVC_REASON, RESTORE_POPULATED_REASON};
    use kopiur_api::RestoreClaimPhase as P;

    let done = claims_of(&[(
        "data",
        mirror_record(
            P::Populated,
            RESTORE_POPULATED_REASON,
            "restored and rebound",
            Some(ResolutionOutcome::Snapshot),
        ),
    )]);
    let status = mirrored_status(&done);
    assert_eq!(
        status["resolved"]["resolution"],
        serde_json::json!("Snapshot")
    );
    assert_eq!(
        condition_of(&status, "Ready").expect("Ready")["reason"],
        RESTORE_POPULATED_REASON
    );

    let mut populating = mirror_record(
        P::Populating,
        POPULATING_PRIME_PVC_REASON,
        "restoring into the prime PVC",
        None,
    );
    populating.pvc_prime = Some("prime-u1".into());
    let status = mirrored_status(&claims_of(&[("data", populating)]));
    assert_eq!(status["target"]["pvcPrime"], "prime-u1", "{status}");
    assert_eq!(status["phase"], "Restoring", "{status}");
}

/// ZERO claims: the park owns that surface, and it must clear a departed single
/// claim's mirrored `pvcRef` — a merge patch merges objects key by key, so
/// writing only the sentinel would leave the gone claim advertised beside it.
#[test]
fn the_zero_claim_park_clears_a_departed_claims_mirror() {
    let empty = std::collections::BTreeMap::new();
    assert_eq!(claims_mirror(&empty), ClaimsMirror::NoClaims);

    let park = awaiting_claim_status(
        &restore_with_anchor(None),
        crate::consts::AWAITING_PVC_DATA_SOURCE_REF_REASON,
        "m",
        &[],
    );
    assert_eq!(park["target"]["pvcPrime"], "awaiting-claim", "{park}");
    assert_eq!(park["target"]["pvcRef"], serde_json::Value::Null, "{park}");
    // Nothing was dropped, so nothing is cleared: a park over a GENUINE
    // pre-fan-out status (no records, a legacy top-level pin) must leave that
    // pin standing for adoption to read.
    assert!(park.get("resolved").is_none(), "{park}");

    // Wave 2, finding 6: dropping the LAST record nulls the top-level mirror in
    // the same patch, so an empty `claims` beside a present `resolved` can only
    // ever mean a genuine pre-fan-out status.
    let last_gone = awaiting_claim_status(
        &restore_with_anchor(None),
        crate::consts::AWAITING_PVC_DATA_SOURCE_REF_REASON,
        "m",
        &["data".to_string()],
    );
    assert_eq!(
        last_gone["resolved"],
        serde_json::Value::Null,
        "{last_gone}"
    );
    assert_eq!(last_gone["claims"]["data"], serde_json::Value::Null);
    assert_eq!(last_gone["target"]["pvcRef"], serde_json::Value::Null);
    assert_eq!(last_gone["target"]["pvcPrime"], "awaiting-claim");
}

/// Wave 2, finding 5 — the single-claim mirror over (claim pinned?, previous
/// top-level pin present?, claim count). `resolved` is the deploy-or-restore
/// DECISION surface and the legacy-adoption signal, so the mirror writes it only
/// when the claim is pinned, nulls it only when the controller is RESETTING
/// (Single→Many, a re-arm, a dropped sibling), and otherwise leaves it alone —
/// an unpinned claim's `null` erased an adopted LEGACY mover's top-level pin
/// before `claim_finalize_pin`'s `own.or(top_level)` could read it.
#[test]
fn the_single_claim_mirror_writes_a_pin_but_never_nulls_an_unpinned_one() {
    use crate::consts::{POPULATING_PRIME_PVC_REASON, RESTORE_POPULATED_REASON};
    use kopiur_api::RestoreClaimPhase as P;

    let pinned = || {
        mirror_record(
            P::Populated,
            RESTORE_POPULATED_REASON,
            "restored",
            Some(ResolutionOutcome::Snapshot),
        )
    };
    let unpinned = || {
        let mut r = mirror_record(P::Populating, POPULATING_PRIME_PVC_REASON, "writing", None);
        r.pvc_prime = Some("prime-u1".into());
        r
    };
    let legacy_pinned = restore_with_legacy_pin();
    let bare = restore_with_anchor(None);
    let none = std::collections::BTreeMap::new();

    for (restore, top_level_present) in [(&legacy_pinned, true), (&bare, false)] {
        // Pinned, one claim ⇒ written (whatever stood there before).
        let one = claims_of(&[("data", pinned())]);
        let s = fanout_status(restore, &none, &one, &[]);
        assert_eq!(
            s["resolved"]["resolution"],
            serde_json::json!("Snapshot"),
            "top_level_present={top_level_present}: {s}"
        );
        assert_eq!(s["target"]["pvcRef"]["name"], "data");

        // Unpinned, one claim ⇒ `resolved` OMITTED — the adopted legacy pin (or
        // nothing) stays. `target` still names the prime being written.
        let one = claims_of(&[("data", unpinned())]);
        let s = fanout_status(restore, &none, &one, &[]);
        assert!(
            s.get("resolved").is_none(),
            "top_level_present={top_level_present}: {s}"
        );
        assert_eq!(s["target"]["pvcPrime"], "prime-u1");
        // …and the same on the heartbeat, where `prev` is that same record.
        let s = fanout_status(restore, &one, &one, &[]);
        assert!(s.get("resolved").is_none(), "{s}");

        // Unpinned, one claim, but the CLAIMANT CHANGED (a re-arm): the pin that
        // stands is the dead claimant's — reset it (finding 2's top-level twin).
        let mut fresh = unpinned();
        fresh.uid = Some("u-new".into());
        let rearmed = claims_of(&[("data", fresh)]);
        let s = fanout_status(restore, &claims_of(&[("data", pinned())]), &rearmed, &[]);
        assert_eq!(s["resolved"], serde_json::Value::Null, "{s}");

        // Unpinned survivor while a SIBLING's record is dropped: the dropped
        // claim's mirrored pin is stale — reset it.
        let s = fanout_status(
            restore,
            &claims_of(&[("data", unpinned()), ("logs", pinned())]),
            &claims_of(&[("data", unpinned())]),
            &["logs".to_string()],
        );
        assert_eq!(s["resolved"], serde_json::Value::Null, "{s}");

        // Two claims from a Single ⇒ nulls; two claims from two ⇒ omitted.
        let mut second = pinned();
        second.uid = Some("u2".into());
        let two = claims_of(&[("data", pinned()), ("logs", second)]);
        let s = fanout_status(restore, &claims_of(&[("data", pinned())]), &two, &[]);
        assert_eq!(s["resolved"], serde_json::Value::Null, "{s}");
        assert_eq!(s["target"], serde_json::Value::Null, "{s}");
        let s = fanout_status(restore, &two, &two, &[]);
        assert!(s.get("resolved").is_none(), "{s}");
    }
}

/// A `Restore` carrying a LEGACY top-level pin (pre-fan-out status shape).
fn restore_with_legacy_pin() -> Restore {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": "r", "namespace": "ns", "generation": 1 },
        "spec": {
            "source": { "fromPolicy": { "name": "cfg" } },
            "target": { "populator": {} }
        },
        "status": {
            "phase": "Restoring",
            "resolved": { "resolution": "Snapshot", "kopiaSnapshotID": "legacy1" },
            "target": { "pvcRef": { "name": "data" }, "pvcPrime": "prime-u1" }
        }
    }))
    .expect("valid Restore")
}

/// SEVERAL claims: there is no single top-level answer, so `resolved`/`target`
/// are CLEARED and the conditions carry the aggregate. A `Restore` that ran with
/// one claim and later gained a second must not keep advertising the first
/// claim's pin as the whole restore's.
#[test]
fn several_claims_clear_the_mirror_and_report_the_aggregate() {
    use crate::consts::RESTORE_POPULATED_REASON;
    use kopiur_api::RestoreClaimPhase as P;

    let mut second = mirror_record(
        P::Populated,
        RESTORE_POPULATED_REASON,
        "restored",
        Some(ResolutionOutcome::NoSnapshot),
    );
    second.uid = Some("u2".into());
    let two = claims_of(&[
        (
            "data",
            mirror_record(
                P::Populated,
                RESTORE_POPULATED_REASON,
                "restored",
                Some(ResolutionOutcome::Snapshot),
            ),
        ),
        ("logs", second),
    ]);
    assert_eq!(claims_mirror(&two), ClaimsMirror::Many);

    // The Single→Many TRANSITION: the previous pass mirrored `data`'s pin, so
    // this pass must explicitly null it (wave 2, finding 5).
    let was_single = claims_of(&[(
        "data",
        mirror_record(
            P::Populated,
            RESTORE_POPULATED_REASON,
            "restored",
            Some(ResolutionOutcome::Snapshot),
        ),
    )]);
    let status = fanout_status(&restore_with_anchor(None), &was_single, &two, &[]);
    assert_eq!(
        status["resolved"],
        serde_json::Value::Null,
        "a two-claim Restore must not advertise one claim's pin as its own: {status}"
    );
    assert_eq!(status["target"], serde_json::Value::Null, "{status}");
    // Many→Many heartbeat: nothing to clear, so `resolved` is not named — a
    // `null` there is what erased an adopted legacy pin (finding 5).
    let steady = fanout_status(&restore_with_anchor(None), &two, &two, &[]);
    assert!(
        steady.get("resolved").is_none(),
        "the many-claim heartbeat must leave `resolved` alone: {steady}"
    );
    let status = mirrored_status(&two);
    assert!(
        condition_of(&status, "Ready").expect("Ready")["message"]
            .as_str()
            .unwrap_or_default()
            .contains("2/2 claims settled"),
        "several claims report the AGGREGATE: {status}"
    );

    // Clearing an already-absent key is a server-side no-op, so the many-claim
    // heartbeat does not re-write just to null what was never there.
    let current = serde_json::json!({
        "phase": status["phase"].clone(),
        "observedGeneration": status["observedGeneration"].clone(),
        "conditions": status["conditions"].clone(),
        "claims": { "data": {}, "logs": {} },
    });
    let mut idempotent = status.clone();
    idempotent["claims"] = serde_json::json!({ "data": {}, "logs": {} });
    assert!(
        crate::io::status_merge_patch_is_noop(Some(&current), &idempotent),
        "nulling absent top-level keys must not force a write: {idempotent}"
    );
}

/// The mirror is CONTROLLER-owned top-level state, so a mirrored
/// `status.resolved` must never make a fanned-out `Restore` look pre-fan-out.
/// `is_legacy_populator_status` keys on `claims` being empty FIRST, and
/// `claim_merge_body` still never names a mover-owned key.
#[test]
fn a_mirrored_pin_is_not_mistaken_for_a_legacy_status() {
    let status: kopiur_api::RestoreStatus = serde_json::from_value(serde_json::json!({
        "phase": "Completed",
        // Exactly what the single-claim mirror writes…
        "resolved": { "resolution": "NoSnapshot", "pinnedAt": "2026-01-01T00:00:00Z" },
        "target": { "pvcRef": { "name": "data" } },
        // …beside the claim it was copied from.
        "claims": { "data": { "phase": "Populated", "reason": "NoSnapshotContinue" } }
    }))
    .expect("valid RestoreStatus");
    assert!(
        !is_legacy_populator_status(&status),
        "a mirrored pin on a Restore that HAS claims is not a legacy status"
    );

    // Wave 2, finding 6: once the LAST claimant is gone, the zero-claim park
    // nulls the mirror together with the record, so the status that reaches the
    // NEXT claimant (empty `claims`, no `resolved`) is not read as pre-fan-out —
    // which would have "adopted" the departed claim's pin for a brand-new PVC.
    let fanned_out: Restore = serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": "r", "namespace": "ns", "generation": 1 },
        "spec": {
            "source": { "fromPolicy": { "name": "cfg" } },
            "target": { "populator": {} }
        },
        "status": serde_json::to_value(&status).unwrap()
    }))
    .expect("valid Restore");
    let park = awaiting_claim_status(
        &fanned_out,
        crate::consts::AWAITING_PVC_DATA_SOURCE_REF_REASON,
        "m",
        &["data".to_string()],
    );
    let after = crate::io::apply_merge_patch(&serde_json::to_value(&status).unwrap(), &park);
    let after: kopiur_api::RestoreStatus = serde_json::from_value(after).expect("merged status");
    assert!(after.claims.is_empty(), "{after:?}");
    assert!(after.resolved.is_none(), "{after:?}");
    assert!(
        !is_legacy_populator_status(&after),
        "a fanned-out Restore whose last claimant left must not adopt as legacy: {after:?}"
    );

    // And the merge body for that claim still names no mover-owned key.
    let record = kopiur_api::RestoreClaimStatus {
        uid: Some("u1".into()),
        phase: Some(kopiur_api::RestoreClaimPhase::Populated),
        resolved: Some(resolved_with(Some(ResolutionOutcome::NoSnapshot), None)),
        ..Default::default()
    };
    let body = claim_merge_body(None, &record);
    for mover_key in ["observedAt", "logTail", "failure"] {
        assert!(body.get(mover_key).is_none(), "{mover_key} in {body}");
    }
}

/// #443 review item 9 — the steady heartbeat skips the cluster-wide
/// `PersistentVolume` LIST, and a prime that will NEVER be reaped must not
/// defeat that. A hijacked populate's prime is kept ON PURPOSE and forever, so
/// treating it as "still busy" buys one cluster-wide LIST every 600 s for a
/// claim whose artifacts nothing will ever collect.
#[test]
fn a_deliberately_kept_prime_does_not_keep_the_pass_busy() {
    use kopiur_api::RestoreClaimPhase as P;
    let live = vec![("data".to_string(), "u1".to_string())];
    let settled = vec![ClaimDrive::Settled];
    let primes: std::collections::BTreeSet<String> = ["prime-u1".to_string()].into();
    let none: std::collections::BTreeSet<String> = Default::default();

    let with = |record: kopiur_api::RestoreClaimStatus| {
        let mut m = std::collections::BTreeMap::new();
        m.insert("data".to_string(), record);
        m
    };

    // A hijacked populate's prime stands forever: quiet.
    let hijacked = with(claim_record(
        Some(P::Failed),
        Some(crate::consts::POPULATE_HIJACKED_REASON),
    ));
    assert!(pass_is_all_quiet(&[], &settled, &live, &hijacked, &primes));

    // A prime that IS reapable means there is real work left (the crashed
    // finalize the Settled sweep exists for), so the LIST is paid.
    let populated = with(claim_record(
        Some(P::Populated),
        Some(crate::consts::RESTORE_POPULATED_REASON),
    ));
    assert!(!pass_is_all_quiet(
        &[],
        &settled,
        &live,
        &populated,
        &primes
    ));
    // No prime at all: quiet.
    assert!(pass_is_all_quiet(&[], &settled, &live, &populated, &none));

    // Anything unsettled, or any record to drop, is never quiet.
    assert!(!pass_is_all_quiet(
        &[],
        &[ClaimDrive::Drive],
        &live,
        &populated,
        &none
    ));
    assert!(!pass_is_all_quiet(
        &["old".to_string()],
        &settled,
        &live,
        &populated,
        &none
    ));
}

/// A claiming PVC, for [`legacy_claimant`]'s table.
fn adoption_claimant(
    name: &str,
    uid: &str,
    bound: bool,
) -> k8s_openapi::api::core::v1::PersistentVolumeClaim {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": { "name": name, "namespace": "ns", "uid": uid },
        "spec": { "volumeName": if bound { "pv-1" } else { "" } },
        "status": { "phase": if bound { "Bound" } else { "Pending" } }
    }))
    .expect("valid PVC")
}

/// #443 review item 4 — the upgrade path's claimant selection, which is the only
/// new logic in it.
///
/// The failing shape is the REPORTER'S OWN cluster after upgrade: N claimants,
/// exactly one populated (the pre-fan-out driver could only ever fill the first
/// LIST hit). Adopting nothing there re-drives the populated claim, which reads
/// `NothingToPopulate` and relabels it `TargetAlreadyBound` — "no restore ran;
/// delete the PVC" — over the volume holding the restored data.
#[test]
fn legacy_adoption_picks_the_one_claim_the_pre_fanout_driver_was_about() {
    let primes = |names: &[&str]| -> std::collections::BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    };

    // 1. A surviving uid-keyed prime names the claim mid-handshake exactly —
    //    bound or not, and regardless of how many claimants there are.
    let mid = vec![
        adoption_claimant("data-a", "uid-a", false),
        adoption_claimant("data-b", "uid-b", true),
    ];
    assert_eq!(
        legacy_claimant(&mid, &primes(&["prime-uid-a"]))
            .map(kube::ResourceExt::name_any)
            .as_deref(),
        Some("data-a")
    );

    // 2. N claimants, no prime, exactly ONE bound: the reporter's state.
    let reporter = vec![
        adoption_claimant("data-a", "uid-a", true),
        adoption_claimant("data-b", "uid-b", false),
        adoption_claimant("data-c", "uid-c", false),
    ];
    assert_eq!(
        legacy_claimant(&reporter, &primes(&[]))
            .map(kube::ResourceExt::name_any)
            .as_deref(),
        Some("data-a"),
        "the populated PVC must be adopted, not relabelled TargetAlreadyBound"
    );

    // 3. A lone BOUND claimant — the ordinary single-PVC upgrade.
    let lone_bound = vec![adoption_claimant("data", "uid", true)];
    assert!(legacy_claimant(&lone_bound, &primes(&[])).is_some());

    // 4. A lone UNBOUND claimant is NOT adopted. That is a `Completed` populator
    //    whose claim was deleted and re-applied; pre-#443 it was re-populated on
    //    the next pass, and adopting `Populated` onto it would settle a claim
    //    that never got its volume.
    let lone_unbound = vec![adoption_claimant("data", "uid", false)];
    assert!(legacy_claimant(&lone_unbound, &primes(&[])).is_none());

    // 5. Several bound, no prime: the old driver could not have populated more
    //    than one, so the extras were bound by something else and we cannot tell
    //    which is ours. Adopt nothing; every claimant drives fresh.
    let ambiguous = vec![
        adoption_claimant("data-a", "uid-a", true),
        adoption_claimant("data-b", "uid-b", true),
    ];
    assert!(legacy_claimant(&ambiguous, &primes(&[])).is_none());
    // 6. None bound at all: nothing was ever populated.
    let none_bound = vec![
        adoption_claimant("data-a", "uid-a", false),
        adoption_claimant("data-b", "uid-b", false),
    ];
    assert!(legacy_claimant(&none_bound, &primes(&[])).is_none());
}

/// #443 review item 8 — adoption must not fire on a brand-new `Restore`. A new
/// populator parks `Pending`/`AwaitingPvcDataSourceRef` with the sentinel
/// `target.pvcPrime: awaiting-claim`; its first claim would otherwise "adopt"
/// that park, spend a Job GET and log that a pre-fan-out status was adopted.
#[test]
fn only_a_pre_fanout_status_is_adopted() {
    let status = |v: serde_json::Value| -> kopiur_api::RestoreStatus {
        serde_json::from_value(v).expect("valid RestoreStatus")
    };

    // The zero-claim park a BRAND-NEW populator writes: not legacy.
    assert!(!is_legacy_populator_status(&status(serde_json::json!({
        "phase": "Pending",
        "target": { "pvcPrime": "awaiting-claim" }
    }))));
    assert!(!is_legacy_populator_status(&status(
        serde_json::json!({ "phase": "Resolving" })
    )));

    // A pinned top-level `resolved` is written only by the pre-fan-out path.
    assert!(is_legacy_populator_status(&status(serde_json::json!({
        "phase": "Pending",
        "resolved": { "kopiaSnapshotID": "k1" }
    }))));
    // …as is any phase past resolution.
    for phase in ["Restoring", "Completed", "Failed"] {
        assert!(
            is_legacy_populator_status(&status(serde_json::json!({ "phase": phase }))),
            "{phase}"
        );
    }
    // A phase this build cannot read is evidence of a NEWER operator, which
    // would have written `claims` — not of a legacy one.
    assert!(!is_legacy_populator_status(&status(
        serde_json::json!({ "phase": "Staging" })
    )));
    // Anything that already has claims is emphatically not legacy.
    assert!(!is_legacy_populator_status(&status(serde_json::json!({
        "phase": "Completed",
        "claims": { "data": { "phase": "Populated" } }
    }))));
}

/// #443 final review, Important 2 — the status alone is not a sufficient
/// adoption signal, so [`legacy_adoption`] adds a second one.
///
/// The pre-fan-out pass was `ensure_prime_pvc` → `run_restore_mover` (creates
/// `{restore}-populate`) → `patch_status(Restoring)`. A `helm upgrade` rollout
/// terminates the old operator at an ARBITRARY instant, so an in-flight populate
/// is routinely left with the prime AND the Job created and the status still
/// `Pending`. Gating on the status alone skipped adoption there and drove the
/// claim `Fresh`, launching a SECOND mover into the prime `{restore}-populate`
/// was still writing.
///
/// `status.target.pvcPrime` cannot be that signal: the legacy driver only ever
/// wrote the `awaiting-claim` sentinel there, never a real prime name.
#[test]
fn an_in_flight_legacy_populate_is_adopted_from_its_live_prime() {
    let status = |v: serde_json::Value| -> kopiur_api::RestoreStatus {
        serde_json::from_value(v).expect("valid RestoreStatus")
    };
    let primes = |names: &[&str]| -> std::collections::BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    };
    let claimants = vec![adoption_claimant("data", "uid-a", false)];

    // 1. A terminal legacy status adopts on the status alone, prime or not.
    for phase in ["Restoring", "Completed", "Failed"] {
        assert_eq!(
            legacy_adoption(
                &status(serde_json::json!({ "phase": phase })),
                &claimants,
                &primes(&[])
            ),
            LegacyAdoption::PreFanoutStatus,
            "{phase}"
        );
    }

    // 2. THE UPGRADE WINDOW: the old operator created `prime-uid-a` and the
    //    `{restore}-populate` Job, then died before writing `Restoring`. The
    //    status is the ordinary zero-claim park — sentinel included, which is
    //    why the sentinel is useless as a signal — and the live prime is the
    //    only evidence there is.
    assert_eq!(
        legacy_adoption(
            &status(serde_json::json!({
                "phase": "Pending",
                "target": { "pvcPrime": "awaiting-claim" }
            })),
            &claimants,
            &primes(&["prime-uid-a"])
        ),
        LegacyAdoption::InFlightPrime
    );
    // …and with no phase written at all, which is the same window one patch
    // earlier.
    assert_eq!(
        legacy_adoption(
            &status(serde_json::json!({})),
            &claimants,
            &primes(&["prime-uid-a"])
        ),
        LegacyAdoption::InFlightPrime
    );

    // 3. A brand-new `Restore`: no prime, no legacy status. Adopting here would
    //    "adopt" the park a zero-claim pass just wrote.
    assert_eq!(
        legacy_adoption(
            &status(serde_json::json!({
                "phase": "Pending",
                "target": { "pvcPrime": "awaiting-claim" }
            })),
            &claimants,
            &primes(&[])
        ),
        LegacyAdoption::No
    );

    // 4. A prime whose claimant is GONE is an orphan for the reaper, not an
    //    in-flight handshake — the prime name is uid-keyed, so it cannot belong
    //    to any live claimant here.
    assert_eq!(
        legacy_adoption(
            &status(serde_json::json!({ "phase": "Pending" })),
            &claimants,
            &primes(&["prime-uid-departed"])
        ),
        LegacyAdoption::No
    );

    // 5. A status that already carries `claims` is a NEWER write, never legacy —
    //    otherwise every ordinary in-flight fan-out pass, which always has a live
    //    prime for a live claimant, would re-adopt itself.
    assert_eq!(
        legacy_adoption(
            &status(serde_json::json!({
                "phase": "Restoring",
                "claims": { "data": { "phase": "Populating" } }
            })),
            &claimants,
            &primes(&["prime-uid-a"])
        ),
        LegacyAdoption::No
    );
}

/// The adopted record must carry the legacy Job even when the status has no
/// phase — that `job` field is the whole point of adopting in the upgrade
/// window, because it is what drives the claim `LegacyShared` instead of
/// `Fresh`. And the `awaiting-claim` PARK SENTINEL must never be copied into the
/// record as if it were a prime name.
#[test]
fn an_adopted_in_flight_claim_records_the_legacy_job_and_drops_the_sentinel() {
    let status = |v: serde_json::Value| -> kopiur_api::RestoreStatus {
        serde_json::from_value(v).expect("valid RestoreStatus")
    };
    let consumer = adoption_claimant("data", "uid-a", false);

    let record = adopt_legacy_claim(
        &status(serde_json::json!({
            "phase": "Pending",
            "target": { "pvcPrime": AWAITING_CLAIM_SENTINEL }
        })),
        &consumer,
        Some("r-populate"),
    )
    .expect("an in-flight legacy claim adopts");
    assert_eq!(record.job.as_deref(), Some("r-populate"));
    assert_eq!(record.phase, Some(kopiur_api::RestoreClaimPhase::Pending));
    assert_eq!(
        record.pvc_prime, None,
        "the park sentinel is not a prime name and must not outlive adoption"
    );
    assert_eq!(record.uid.as_deref(), Some("uid-a"));

    // No phase at all still adopts, conservatively, as `Pending`.
    let no_phase = adopt_legacy_claim(
        &status(serde_json::json!({})),
        &consumer,
        Some("r-populate"),
    )
    .expect("a phase-less legacy status still adopts");
    assert_eq!(no_phase.phase, Some(kopiur_api::RestoreClaimPhase::Pending));
    assert_eq!(no_phase.job.as_deref(), Some("r-populate"));

    // A REAL prime name is still carried through untouched.
    let real = adopt_legacy_claim(
        &status(serde_json::json!({
            "phase": "Restoring",
            "target": { "pvcPrime": "prime-uid-a" }
        })),
        &consumer,
        None,
    )
    .expect("adopts");
    assert_eq!(real.pvc_prime.as_deref(), Some("prime-uid-a"));
}

/// #443 review item 5 — an adopted `{restore}-populate` Job carries no
/// `claimKey`, so when its DEFERRED source resolves the mover pins the
/// TOP-LEVEL `status.resolved`. Adoption snapshots that field exactly once (when
/// it is typically still unpinned), so without a fallback the finalize reads
/// `None` and records "provisioned an empty volume" over a real restore.
///
/// A `Fresh` claim must NEVER take that fallback: its mover writes only
/// `status.claims.<pvc>.resolved`, and reading the top level would let it
/// inherit a sibling's — or a legacy — pin and report the wrong snapshot.
#[test]
fn only_an_adopted_legacy_job_reads_the_top_level_pin() {
    let snapshot = resolved_with(Some(ResolutionOutcome::Snapshot), Some("k9"));
    let empty = resolved_with(Some(ResolutionOutcome::NoSnapshot), None);

    // LegacyShared with nothing of its own: the top-level pin its own mover wrote.
    assert_eq!(
        claim_finalize_pin(
            JobNameReuse::LegacyShared,
            None,
            None,
            Some(snapshot.clone())
        ),
        Some(snapshot.clone())
    );
    // Fresh with nothing of its own: NO fallback — an absent pin stays absent.
    assert_eq!(
        claim_finalize_pin(JobNameReuse::Fresh, None, None, Some(snapshot.clone())),
        None
    );
    // The claim's own pin always wins over both.
    for reuse in [JobNameReuse::Fresh, JobNameReuse::LegacyShared] {
        assert_eq!(
            claim_finalize_pin(
                reuse,
                Some(empty.clone()),
                Some(snapshot.clone()),
                Some(snapshot.clone())
            ),
            Some(empty.clone())
        );
        // …then the record carried into the pass.
        assert_eq!(
            claim_finalize_pin(reuse, None, Some(empty.clone()), Some(snapshot.clone())),
            Some(empty.clone())
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
        restore_ready_outcome(&RestorePhase::Completed),
        ReadyOutcome::Ready
    );
    assert_eq!(
        restore_ready_outcome(&RestorePhase::Failed),
        ReadyOutcome::Stalled
    );
    for p in [
        RestorePhase::Pending,
        RestorePhase::Resolving,
        RestorePhase::Restoring,
        // Never Ready, never Stalled — `kubectl wait` keeps waiting.
        RestorePhase::Unknown("Staging".into()),
    ] {
        assert_eq!(
            restore_ready_outcome(&p),
            ReadyOutcome::Reconciling,
            "{p:?}"
        );
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

    assert!(!kstatus_settled_for(&r, &RestorePhase::Completed));
    assert!(!kstatus_settled_for(&r, &RestorePhase::Failed));

    // Heal (what the terminal gate patches), then it must be settled.
    let healed = restore_ready_status(&r, RestorePhase::Completed, "RestoreSucceeded", "done");
    let mut status = r.status.take().unwrap();
    status.conditions = serde_json::from_value(healed["conditions"].clone()).unwrap();
    r.status = Some(status);
    assert!(kstatus_settled_for(&r, &RestorePhase::Completed));
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

/// The `waitTimeout` window is anchored at `status.waitStartedAt` — the instant the
/// restore could first PROCEED — and falls back to the Restore's creation only while no
/// anchor has been stamped (#380). Precedence matrix: unset, set, stamped-before-creation.
#[test]
fn effective_wait_anchor_prefers_the_stamped_window_start() {
    // 2026-01-01T00:00:00Z == 1767225600. The Restore itself was created long before.
    let created = 1_000_000_000;

    // Unset → the creation timestamp (the pre-#380 behavior, and what a Restore that has
    // not yet cleared the readiness gate still reads).
    assert_eq!(
        effective_wait_anchor(&restore_with_anchor(None), created),
        created
    );

    // Stamped → the stamp wins, so the window measures from when it OPENED.
    assert_eq!(
        effective_wait_anchor(&restore_with_anchor(Some("2026-01-01T00:00:00Z")), created),
        1_767_225_600
    );
    // Non-UTC offsets are honored (RFC3339, not a fixed `Z` shape).
    assert_eq!(
        effective_wait_anchor(
            &restore_with_anchor(Some("2026-01-01T01:00:00+01:00")),
            created
        ),
        1_767_225_600
    );

    // An anchor that predates creation never SHORTENS the window (hand-edited status,
    // clock skew): the change is one-directional — windows only ever extend.
    assert_eq!(
        effective_wait_anchor(&restore_with_anchor(Some("2001-09-09T01:46:40Z")), created),
        created
    );
    // Garbage is inert rather than fatal.
    assert_eq!(
        effective_wait_anchor(&restore_with_anchor(Some("not-a-timestamp")), created),
        created
    );
}

/// The regression this whole change exists for: the mover's absolute wait deadline is
/// `anchor + waitTimeout`, NOT `creation + waitTimeout`. A Restore parked for a week on a
/// not-Ready repository (or a populator with no claim) must still get its full window on
/// the pass that finally opens it — otherwise a `fromPolicy` source, defaulting to
/// `onMissingSnapshot: Continue`, provisions an EMPTY volume instantly.
#[test]
fn wait_deadline_runs_from_the_anchor_not_from_creation() {
    let created = 1_000_000_000; // long ago
    let opened = 1_767_225_600; // 2026-01-01T00:00:00Z — the gate finally cleared
    let restore = restore_with_anchor(Some("2026-01-01T00:00:00Z"));
    let anchor = effective_wait_anchor(&restore, created);

    // The deadline the mover polls against: anchor + 5m.
    assert_eq!(
        wait_deadline_rfc3339(anchor, Some("5m")).as_deref(),
        Some("2026-01-01T00:05:00+00:00")
    );
    // Anchored at creation it would have closed in 2001 — the pre-fix bug.
    assert!(
        wait_deadline_rfc3339(created, Some("5m")).unwrap()
            < wait_deadline_rfc3339(anchor, Some("5m")).unwrap()
    );

    // ...and the controller-side wait agrees: the full 5m remains one second after the
    // window opened, where the creation-anchored window had long since elapsed.
    assert_eq!(
        wait_remaining_secs(anchor, Some("5m"), opened + 60),
        Some(240)
    );
    assert_eq!(wait_remaining_secs(created, Some("5m"), opened + 60), None);

    // No window configured / unparseable ⇒ no deadline at all (unchanged).
    assert_eq!(wait_deadline_rfc3339(anchor, None), None);
    assert_eq!(wait_deadline_rfc3339(anchor, Some("later")), None);
}

/// Which target modes may OPEN the window: a direct target the moment the repository is
/// Ready, a populator only once a PVC claims it. Resolution runs while a populator is
/// `AwaitingClaim`, so a standing GitOps populator created long before its claim would
/// otherwise spend the whole window idle and pin `Empty` the instant a claim appeared.
#[test]
fn wait_window_opens_for_a_populator_only_once_a_claim_exists() {
    use PopulatorState::{AwaitingClaim, DirectTarget};
    assert!(wait_window_opens(DirectTarget, false));
    assert!(wait_window_opens(DirectTarget, true));
    assert!(!wait_window_opens(AwaitingClaim, false));
    assert!(wait_window_opens(AwaitingClaim, true));
}

/// Parking inside the wait window reports the REAL blocker. An unclaimed populator reaches
/// the wait branch (resolution runs while `AwaitingClaim`), and telling that user to read
/// `status.waitStartedAt` — deliberately absent until a claim appears — points them at the
/// wrong thing. It also never resolves on its own, so it takes the 30s awaiting-claim
/// cadence rather than a permanent 15s poll.
#[test]
fn wait_park_report_names_the_blocker_and_picks_the_cadence() {
    let (reason, msg, requeue) = wait_park_report(WaitWindow::Open(1000), Some("5m"), 240);
    assert_eq!(reason, "WaitingForSnapshot");
    assert!(msg.contains("no snapshot matched"), "{msg}");
    assert!(msg.contains("waitTimeout (5m)"), "{msg}");
    assert!(msg.contains("status.waitStartedAt"), "{msg}");
    assert_eq!(requeue, 15, "the wait cadence is capped at 15s");
    // ...but never past the deadline.
    assert_eq!(wait_park_report(WaitWindow::Open(1000), Some("5m"), 3).2, 3);
    assert_eq!(wait_park_report(WaitWindow::Open(1000), Some("5m"), 0).2, 1);

    let (reason, msg, requeue) = wait_park_report(WaitWindow::AwaitingClaim(1000), Some("5m"), 240);
    assert_eq!(reason, "AwaitingPvcDataSourceRef");
    assert!(
        msg.contains("dataSourceRef") && msg.contains("Create the claiming PVC"),
        "the message must name the real blocker and the fix: {msg}"
    );
    assert!(
        msg.contains("has NOT started"),
        "it must say the window has not started, not imply a snapshot wait: {msg}"
    );
    assert!(
        !msg.contains("no snapshot matched"),
        "an unclaimed populator is not waiting on a snapshot: {msg}"
    );
    assert_eq!(
        requeue, 30,
        "the awaiting-claim cadence, not the wait cadence"
    );

    // Whichever state, the anchor the caller measures with is the one it carries.
    assert_eq!(WaitWindow::Open(1000).anchor(), 1000);
    assert_eq!(WaitWindow::AwaitingClaim(7).anchor(), 7);
}

/// A populator claim that is deleted and re-created must measure its
/// `waitTimeout` from the NEW claim, not from an anchor spent on the previous
/// one — otherwise the window is already gone, and a `fromPolicy` source (which
/// defaults to `Continue`) skips the wait and provisions an EMPTY volume the
/// instant the snapshot happens not to be there yet.
///
/// Before #443 that took an explicit JSON `null` on the Restore's top-level
/// `waitStartedAt` (a merge patch deletes only the keys it names). The fan-out
/// makes it structural instead: a re-created claimant has a NEW uid, so
/// `claim_drive` re-arms it and the driver writes a fresh record — the spent
/// anchor goes with the record it belonged to, and `claim_merge_body` emits the
/// null for it.
#[test]
fn a_re_armed_claim_drops_the_previous_claims_spent_wait_anchor() {
    use kopiur_api::{RestoreClaimPhase as P, RestoreClaimStatus};

    let spent = RestoreClaimStatus {
        uid: Some("old-uid".into()),
        phase: Some(P::Populated),
        wait_started_at: Some("2026-01-01T00:00:00Z".into()),
        ..Default::default()
    };
    // A new claimant uid re-arms rather than settling, even though the record is
    // terminal — that is what makes re-creating the PVC the documented retry.
    assert_eq!(
        claim_drive(Some(&spent), "new-uid"),
        ClaimDrive::ReArm {
            stale_uid: "old-uid".into()
        }
    );
    // The fresh record carries no anchor, and the merge body NULLS the old one:
    // an elided `None` would leave a window that closed months ago in place.
    let fresh = RestoreClaimStatus {
        uid: Some("new-uid".into()),
        phase: Some(P::Pending),
        reason: Some(ClaimReason::ClaimRecreated.as_str().into()),
        ..Default::default()
    };
    let body = claim_merge_body(Some(&spent), &fresh);
    assert_eq!(
        body.get("waitStartedAt"),
        Some(&serde_json::Value::Null),
        "the clear must be an EXPLICIT null, not an omitted key: {body}"
    );
    assert_eq!(
        body.get("uid").and_then(|u| u.as_str()),
        Some("new-uid"),
        "{body}"
    );

    // Review wave 2, finding 2: the re-arm is a RESET of the dead claimant's
    // whole record, and the controller owns the reset even though the mover
    // owns these values in steady state. Stripping (not nulling) them left the
    // previous claim's pin standing, so `claim_pinned_decision` short-circuited
    // and a re-created PVC under `Continue` was provisioned EMPTY again although
    // a snapshot now matched — or restored a stale mover pin, or showed a stale
    // failure block beside a fresh `Pending`.
    let spent_with_mover_state: RestoreClaimStatus = serde_json::from_value(serde_json::json!({
        "uid": "old-uid",
        "phase": "Populated",
        "reason": "NoSnapshotContinue",
        "waitStartedAt": "2026-01-01T00:00:00Z",
        "resolved": { "resolution": "NoSnapshot", "pinnedAt": "2026-01-01T00:00:00Z" },
        "observedAt": "2026-01-01T00:05:00Z",
        "logTail": "kopia: nothing to restore",
        "failure": { "kopiaErrorClass": "Unknown", "message": "x", "retryRecommended": false }
    }))
    .expect("valid claim record");
    let body = claim_merge_body(Some(&spent_with_mover_state), &fresh);
    for reset in ["resolved", "failure", "logTail", "observedAt"] {
        assert_eq!(
            body.get(reset),
            Some(&serde_json::Value::Null),
            "a re-arm must EXPLICITLY null `{reset}`: {body}"
        );
    }
    // The nulls are scoped to the re-arm: the SAME claimant's heartbeat still
    // never names a mover-owned key (the mover writes them; a controller pass
    // must not blank them), and keeps a pin it did not itself drop.
    let same_uid_next = RestoreClaimStatus {
        uid: Some("old-uid".into()),
        phase: Some(P::Populating),
        reason: Some(ClaimReason::PopulatingPrimePvc.as_str().into()),
        ..Default::default()
    };
    let body = claim_merge_body(Some(&spent_with_mover_state), &same_uid_next);
    for untouched in ["resolved", "failure", "logTail", "observedAt"] {
        assert!(
            body.get(untouched).is_none(),
            "same claimant: `{untouched}` must be left to its owner, got {body}"
        );
    }

    // A claim window with no record and no legacy top-level anchor opens at `now`,
    // so a claimant that appears an hour after its sibling gets its OWN full window.
    assert_eq!(
        claim_wait_window(None, &restore_with_anchor(None), 1_700_000_000),
        WaitWindow::Open(1_700_000_000)
    );
}

/// A `Restore` carrying `status.waitStartedAt` (or not).
fn restore_with_anchor(wait_started_at: Option<&str>) -> Restore {
    let mut status = serde_json::json!({ "phase": "Pending" });
    if let Some(at) = wait_started_at {
        status["waitStartedAt"] = serde_json::Value::String(at.to_string());
    }
    serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": "r", "namespace": "ns", "generation": 1 },
        "spec": {
            "source": { "fromPolicy": { "name": "cfg" } },
            "target": { "populator": {} }
        },
        "status": status
    }))
    .expect("valid Restore")
}

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

#[test]
fn absent_restore_target_pvc_stays_a_transient_race() {
    // #382 M5 per-caller mapping: the restore TARGET PVC was ensured moments
    // before the co-location read, so a 404 is a race — a transient
    // MissingDependency retry, never the Snapshot-side MissingSourcePvc
    // gate/deadline machinery.
    let err = restore_target_pvc_race_error("app", "restored-data");
    assert!(matches!(&err, Error::MissingDependency(_)), "{err:?}");
    assert_eq!(err.class(), crate::error::ErrorClass::Transient);
    assert!(err.to_string().contains("app/restored-data"));
    assert!(err.to_string().contains("race"));
}

// --- the restore's repository mover-Job pool reservation ----------------------
//
// The reconciler-level half of the P1 guard (`crate::pool` owns the ledger's own
// truth table). What can only be checked HERE is that the restore path takes a
// slot at all, and takes it under the key the observed-Job sweep will match: the
// name of the Job this dispatch actually applies. `run_restore_mover` threads ONE
// `job_name` binding into both `reserve_restore_slot` and `apply_mover_objects`,
// and these tests pin both flavors of that name — the direct restore's
// (`{restore}`) and the populator's (`{restore}-populate`).

/// A resolved namespaced `Repository` named `nas` in `backups`, with `cap` as its
/// `spec.concurrency.maxConcurrentJobs` (`None` = uncapped, the default install).
fn pooled_repo(cap: Option<u32>) -> crate::io::ResolvedRepository {
    use kopiur_api::backend::{Backend, FilesystemBackend};
    use kopiur_api::common::{Encryption, RepositoryKind, SecretKeyRef};
    crate::io::ResolvedRepository {
        backend: Backend::Filesystem(FilesystemBackend {
            path: "/repo".into(),
            volume: None,
        }),
        mover_defaults: None,
        encryption: Encryption {
            password_secret_ref: SecretKeyRef {
                name: "creds".into(),
                namespace: None,
                key: None,
            },
        },
        kind: RepositoryKind::Repository,
        repo_namespace: Some("backups".into()),
        identity_defaults: None,
        schedule_defaults: None,
        on_namespace_delete: Default::default(),
        mode: Default::default(),
        credential_projection_allowed: false,
        // `repository_ref()` reads the repository's NAME off here, and the pool
        // key is a hash of it — a blank one would silently key every test repo
        // to the same pool.
        owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            name: "nas".into(),
            ..Default::default()
        },
        deletion_protection: None,
        concurrency: cap.map(|c| kopiur_api::common::ConcurrencySpec {
            max_concurrent_jobs: Some(c),
        }),
        mass_deletion_ack: None,
        catalog: None,
        ca_bundle_pem: None,
    }
}

/// A `kube::Client` that answers every request with an EMPTY `JobList` — the
/// pool a first-instant admission actually observes, and the state a concurrent
/// backup would read while this restore's Job does not exist yet.
fn empty_job_list_client() -> kube::Client {
    use http::Response;
    use kube::client::Body;
    let svc = tower::service_fn(move |_req: http::Request<Body>| async move {
        let body = serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "JobList",
            "metadata": {},
            "items": [],
        })
        .to_string();
        Ok::<_, std::convert::Infallible>(
            Response::builder()
                .status(http::StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from(body.into_bytes()))
                .expect("response"),
        )
    });
    kube::Client::new(svc, "default")
}

/// The reservation the restore gate records for `job_name`, as
/// `pool key → job keys`.
async fn restore_reservation_keys(
    ctx: &Context,
    repo: &crate::io::ResolvedRepository,
    job_name: &str,
) -> Vec<(String, Vec<String>)> {
    let slot = reserve_restore_slot(ctx, repo, "apps", job_name)
        .await
        .expect("the restore gate never fails on a healthy LIST");
    assert!(
        slot.is_some(),
        "a capped repository must hand the restore a slot guard to hold"
    );
    let held = ctx.pool_admissions.outstanding_for_test();
    // Drop AFTER reading: a guard released at the end of its statement would
    // make every assertion below pass against a gate that reserved nothing.
    drop(slot);
    assert!(
        ctx.pool_admissions.outstanding_for_test().is_empty(),
        "the guard must release the restore's slot on drop"
    );
    held.into_iter()
        .map(|(pool, jobs)| (pool, jobs.into_iter().collect()))
        .collect()
}

#[tokio::test]
async fn a_direct_restore_reserves_its_slot_under_its_own_job_name() {
    // A direct restore's mover Job is named after the `Restore` itself, so the
    // reservation must be keyed `{namespace}/{restore}` — anything else is a
    // promise the observed-Job sweep can never retire.
    let ctx = Context::test_context(empty_job_list_client());
    let repo = pooled_repo(Some(1));
    assert_eq!(
        restore_reservation_keys(&ctx, &repo, "db-recovery").await,
        vec![(
            crate::naming::repo_label(&repo.repository_ref()),
            vec!["apps/db-recovery".to_string()],
        )],
    );
}

#[tokio::test]
async fn a_populating_restore_reserves_its_slot_under_the_populate_job_name() {
    // The populator's Job is `{restore}-populate`, NOT the Restore's own name.
    // Keying the reservation off the CR here was the drift this pins against:
    // the sweep matches `ObservedPool::seen`, which holds Job names.
    let ctx = Context::test_context(empty_job_list_client());
    let repo = pooled_repo(Some(1));
    assert_eq!(
        restore_reservation_keys(&ctx, &repo, "db-recovery-populate").await,
        vec![(
            crate::naming::repo_label(&repo.repository_ref()),
            vec!["apps/db-recovery-populate".to_string()],
        )],
    );
}

#[tokio::test]
async fn a_reserved_restore_slot_parks_a_concurrent_backup_at_a_cap_of_one() {
    // The P1 at the reconciler's own gate: while the restore holds its slot —
    // and the LIST still shows an EMPTY pool, because the restore's Job does
    // not exist yet — a `Snapshot` arriving at the same repository must park.
    let ctx = Context::test_context(empty_job_list_client());
    let repo = pooled_repo(Some(1));
    let pool_key = crate::naming::repo_label(&repo.repository_ref());
    let caps = crate::pool::PoolCaps {
        repo: std::num::NonZeroUsize::new(1),
        global: None,
    };

    let slot = reserve_restore_slot(&ctx, &repo, "apps", "db-recovery")
        .await
        .expect("restore gate")
        .expect("a capped repository hands out a guard");
    let verdict = crate::pool::admit_or_park(
        &ctx,
        &pool_key,
        "apps/nightly",
        crate::pool::PoolClass::Backup,
        caps,
    )
    .await
    .expect("backup gate");
    assert!(
        matches!(
            verdict,
            crate::pool::LedgerVerdict::Park {
                repo_live: 1,
                global_live: 1,
            }
        ),
        "a backup ran beside an in-flight restore: {verdict:?}"
    );

    // Once the restore's window closes, the slot is the backup's.
    drop(slot);
    assert!(matches!(
        crate::pool::admit_or_park(
            &ctx,
            &pool_key,
            "apps/nightly",
            crate::pool::PoolClass::Backup,
            caps,
        )
        .await
        .expect("backup gate"),
        crate::pool::LedgerVerdict::Admit { .. }
    ));
}

#[tokio::test]
async fn an_uncapped_repository_leaves_the_restore_path_untouched() {
    // The default install: no cap anywhere, so the restore gate makes no API
    // call, takes no lock and records nothing. `None` here is "no budget",
    // never "held" — a restore has no park outcome to represent.
    let ctx = Context::test_context(empty_job_list_client());
    let slot = reserve_restore_slot(&ctx, &pooled_repo(None), "apps", "db-recovery")
        .await
        .expect("restore gate");
    assert!(slot.is_none());
    assert!(ctx.pool_admissions.outstanding_for_test().is_empty());
}

// --- #443: the fanned-out populator's per-claim decisions -------------------

use kopiur_api::{RestoreClaimPhase, RestoreClaimStatus};
use std::collections::BTreeMap;

/// A claim record with just a phase + reason — the shape the aggregate reads.
fn claim(phase: Option<RestoreClaimPhase>, reason: Option<&str>) -> RestoreClaimStatus {
    RestoreClaimStatus {
        uid: Some("u".into()),
        phase,
        reason: reason.map(str::to_string),
        ..Default::default()
    }
}

fn claims(entries: &[(&str, RestoreClaimStatus)]) -> BTreeMap<String, RestoreClaimStatus> {
    entries
        .iter()
        .map(|(n, r)| ((*n).to_string(), r.clone()))
        .collect()
}

#[test]
fn claim_class_is_exhaustive_over_every_claim_phase() {
    use RestoreClaimPhase as P;
    use kopiur_api::common::PhaseLabel;
    // Driven off ALL, so a new claim phase must be classified deliberately.
    let classified: Vec<(&str, ClaimClass)> = P::ALL
        .iter()
        .map(|p| (p.label(), claim_class(Some(p))))
        .collect();
    assert_eq!(
        classified,
        [
            ("Pending", ClaimClass::InFlight),
            ("Populating", ClaimClass::InFlight),
            ("Rebinding", ClaimClass::InFlight),
            ("Populated", ClaimClass::Populated),
            ("AlreadyBound", ClaimClass::AlreadyBound),
            ("Failed", ClaimClass::Failed),
        ]
    );
    // An unobserved record and an unreadable phase are both IN FLIGHT — never
    // settled, so a newer operator's claim cannot make the Restore report
    // `Completed` over work that may still be running.
    assert_eq!(claim_class(None), ClaimClass::InFlight);
    assert_eq!(
        claim_class(Some(&P::Unknown("Quiescing".into()))),
        ClaimClass::InFlight
    );
}

#[test]
fn aggregate_covers_the_four_states_of_a_fan_out() {
    use RestoreClaimPhase as P;

    // Nothing claims the populator yet.
    assert_eq!(
        aggregate_claims(&BTreeMap::new()),
        ClaimsAggregate::NoClaims
    );
    assert_eq!(
        aggregate_phase(&ClaimsAggregate::NoClaims),
        RestorePhase::Pending
    );

    // Every claim settled, none failed → the Restore completed.
    let all = claims(&[
        ("a", claim(Some(P::Populated), Some("RestoreSucceeded"))),
        (
            "b",
            claim(Some(P::AlreadyBound), Some("TargetAlreadyBound")),
        ),
    ]);
    let agg = aggregate_claims(&all);
    let ClaimsAggregate::AllSettled(t) = &agg else {
        panic!("expected AllSettled, got {agg:?}");
    };
    assert_eq!((t.total, t.populated, t.already_bound), (2, 1, 1));
    assert_eq!(aggregate_phase(&agg), RestorePhase::Completed);

    // A mover running → `Restoring`.
    let running = claims(&[
        ("a", claim(Some(P::Populated), Some("RestoreSucceeded"))),
        ("b", claim(Some(P::Populating), Some("PopulatingPrimePvc"))),
    ]);
    let agg = aggregate_claims(&running);
    assert!(matches!(agg, ClaimsAggregate::InFlight(_)));
    assert_eq!(aggregate_phase(&agg), RestorePhase::Restoring);

    // …and a rebind outstanding counts as active too.
    let rebinding = claims(&[("a", claim(Some(P::Rebinding), None))]);
    assert_eq!(
        aggregate_phase(&aggregate_claims(&rebinding)),
        RestorePhase::Restoring
    );

    // Everything merely WAITING → `Pending`, not `Restoring`: nothing of ours is
    // running, and reporting `Restoring` over an unscheduled claimant would make
    // a stuck fan-out look busy.
    let waiting = claims(&[
        ("a", claim(Some(P::Pending), Some("AwaitingPodSchedule"))),
        ("b", claim(None, None)),
    ]);
    let agg = aggregate_claims(&waiting);
    assert!(matches!(agg, ClaimsAggregate::InFlight(_)));
    assert_eq!(aggregate_phase(&agg), RestorePhase::Pending);

    // One failure stalls the Restore even while siblings still run.
    let mixed = claims(&[
        ("a", claim(Some(P::Populated), Some("RestoreSucceeded"))),
        ("b", claim(Some(P::Populating), Some("PopulatingPrimePvc"))),
        ("c", claim(Some(P::Failed), Some("MoverJobFailed"))),
    ]);
    let agg = aggregate_claims(&mixed);
    let ClaimsAggregate::Failed(t) = &agg else {
        panic!("expected Failed, got {agg:?}");
    };
    assert_eq!(t.failed, [("c".to_string(), "MoverJobFailed".to_string())]);
    assert_eq!(t.in_flight, ["b".to_string()]);
    assert_eq!(aggregate_phase(&agg), RestorePhase::Failed);
    // kstatus follows from the phase, through the unchanged mapping.
    assert_eq!(
        restore_ready_outcome(&aggregate_phase(&agg)),
        crate::io::ReadyOutcome::Stalled
    );
    assert_eq!(
        restore_ready_outcome(&aggregate_phase(&aggregate_claims(&all))),
        crate::io::ReadyOutcome::Ready
    );

    // An unreadable claim phase never settles the Restore.
    let unknown = claims(&[
        ("a", claim(Some(P::Populated), None)),
        ("b", claim(Some(P::Unknown("Quiescing".into())), None)),
    ]);
    assert_eq!(
        aggregate_phase(&aggregate_claims(&unknown)),
        RestorePhase::Pending
    );
}

#[test]
fn claims_summary_names_the_counts_the_stragglers_and_the_failures() {
    use RestoreClaimPhase as P;
    let mixed = claims(&[
        ("a", claim(Some(P::Populated), Some("RestoreSucceeded"))),
        ("b", claim(Some(P::Populated), Some("RestoreSucceeded"))),
        ("c", claim(Some(P::Populating), Some("PopulatingPrimePvc"))),
        ("d", claim(Some(P::Failed), Some("MoverJobFailed"))),
    ]);
    let (reason, message) = claims_summary(&aggregate_claims(&mixed));
    assert_eq!(reason, crate::consts::RESTORE_CLAIM_FAILED_REASON);
    assert!(message.starts_with("2/4 claims settled"), "{message}");
    assert!(message.contains("populated: 2"), "{message}");
    // "in flight", not "populating": the list carries Pending claims too.
    assert!(message.contains("in flight: c"), "{message}");
    assert!(message.contains("failed: d (MoverJobFailed)"), "{message}");
    // The fix a human acts on: siblings keep going, re-create the failed claim.
    assert!(message.contains("re-create that claiming PVC"), "{message}");

    // The healthy states carry the reasons the pre-#443 single-claim path used,
    // so `kubectl describe` reads the same for a one-PVC populator.
    let (reason, message) = claims_summary(&aggregate_claims(&claims(&[(
        "a",
        claim(Some(P::Populated), None),
    )])));
    assert_eq!(reason, crate::consts::RESTORE_POPULATED_REASON);
    assert_eq!(message, "1/1 claims settled; populated: 1");

    let (reason, _) = claims_summary(&aggregate_claims(&claims(&[(
        "a",
        claim(Some(P::Populating), None),
    )])));
    assert_eq!(reason, crate::consts::POPULATING_PRIME_PVC_REASON);

    // An already-bound no-op is counted separately from a real populate — saying
    // "1/1 populated" over a claim nothing was written to would be a lie.
    let (_, message) = claims_summary(&aggregate_claims(&claims(&[(
        "a",
        claim(Some(P::AlreadyBound), None),
    )])));
    // Counted as SETTLED (it is a success) but never as populated — saying
    // "1/1 populated" over a claim nothing was written to would be a lie, and
    // "0/1 settled" beside Ready=True reads as a contradiction.
    assert_eq!(message, "1/1 claims settled; already bound: 1");

    // Zero claims points at the missing dataSourceRef, and says the window has
    // not started.
    let (reason, message) = claims_summary(&ClaimsAggregate::NoClaims);
    assert_eq!(reason, crate::consts::AWAITING_PVC_DATA_SOURCE_REF_REASON);
    assert!(message.contains("dataSourceRef"), "{message}");
    assert!(message.contains("has NOT started"), "{message}");
}

#[test]
fn claim_drive_has_exactly_three_outcomes() {
    use RestoreClaimPhase as P;
    // No record yet → drive it.
    assert_eq!(claim_drive(None, "live"), ClaimDrive::Drive);

    // Record for a DELETED claimant → re-arm, naming the dead uid so its prime
    // PVC / Job / PV can be reaped under it.
    let stale = RestoreClaimStatus {
        uid: Some("dead".into()),
        phase: Some(P::Populated),
        ..Default::default()
    };
    assert_eq!(
        claim_drive(Some(&stale), "live"),
        ClaimDrive::ReArm {
            stale_uid: "dead".into()
        }
    );

    // Terminal under the LIVE uid → settled, for all three terminal phases. A
    // `Failed` claim is UNBOUND with its prime still present, so a "bound and no
    // prime" test would re-drive it forever on the 120s cadence.
    for phase in [P::Populated, P::AlreadyBound, P::Failed] {
        let settled = RestoreClaimStatus {
            uid: Some("live".into()),
            phase: Some(phase.clone()),
            ..Default::default()
        };
        assert_eq!(claim_drive(Some(&settled), "live"), ClaimDrive::Settled);
    }

    // Non-terminal under the live uid → drive.
    for phase in [
        None,
        Some(P::Pending),
        Some(P::Populating),
        Some(P::Rebinding),
    ] {
        let live = RestoreClaimStatus {
            uid: Some("live".into()),
            phase,
            ..Default::default()
        };
        assert_eq!(claim_drive(Some(&live), "live"), ClaimDrive::Drive);
    }
    // An unreadable phase is driven, never settled.
    let unknown = RestoreClaimStatus {
        uid: Some("live".into()),
        phase: Some(P::Unknown("Quiescing".into())),
        ..Default::default()
    };
    assert_eq!(claim_drive(Some(&unknown), "live"), ClaimDrive::Drive);

    // A record with no uid has nothing to reap: drive rather than re-arm.
    let uidless = RestoreClaimStatus {
        uid: None,
        phase: Some(P::Pending),
        ..Default::default()
    };
    assert_eq!(claim_drive(Some(&uidless), "live"), ClaimDrive::Drive);
}

#[test]
fn claim_reason_vocabulary_round_trips_and_a_hijack_or_an_unreadable_reason_blocks_the_reaper() {
    // Every reason parses back to itself, so a write and a later compare cannot
    // drift.
    for r in ClaimReason::ALL {
        assert_eq!(ClaimReason::parse(r.as_str()), Some(*r), "{}", r.as_str());
    }
    // Labels are unique — two reasons sharing a string would make `parse`
    // silently answer the wrong one.
    let mut labels: Vec<&str> = ClaimReason::ALL.iter().map(|r| r.as_str()).collect();
    let count = labels.len();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), count, "claim reasons must be unique");

    // The one destructive decision: a hijacked populate's prime PVC holds
    // HALF-WRITTEN data and must survive the reaper. Before #443 what protected
    // it was the whole populator short-circuiting on `Failed` at the guard.
    assert!(!ClaimReason::PopulateHijacked.artifacts_reapable());
    assert!(!claim_artifacts_reapable(Some(
        crate::consts::POPULATE_HIJACKED_REASON
    )));
    for r in ClaimReason::ALL
        .iter()
        .filter(|r| **r != ClaimReason::PopulateHijacked)
    {
        assert!(r.artifacts_reapable(), "{} must be reapable", r.as_str());
        assert!(claim_artifacts_reapable(Some(r.as_str())));
    }
    // Wave 2, finding 7 — FAIL CLOSED on a reason this build cannot read. An
    // absent reason is a half-written or hand-patched record; an unknown string
    // is a NEWER operator's vocabulary, which may well be its own "keep the
    // prime" case. Deleting a prime PVC on a guess is irreversible, so neither
    // is reaped automatically: a human deletes `prime-<uid>` by hand.
    assert!(!claim_artifacts_reapable(None));
    assert!(!claim_artifacts_reapable(Some("")));
    assert!(!claim_artifacts_reapable(Some("SomethingNewerWrote")));
    // …and the settled-sweep gate inherits the same answer.
    for reason in [None, Some("SomethingNewerWrote")] {
        let record = RestoreClaimStatus {
            uid: Some("u1".into()),
            phase: Some(kopiur_api::RestoreClaimPhase::Populated),
            reason: reason.map(str::to_string),
            ..Default::default()
        };
        assert!(
            !settled_artifacts_reapable(&record),
            "reason={reason:?} must never be swept automatically"
        );
    }
}

/// A legacy (pre-fan-out) `Restore` status: top-level phase + a `Ready`
/// condition, with an optional pinned resolution and wait anchor.
fn legacy_status(phase: &str, reason: &str) -> kopiur_api::RestoreStatus {
    serde_json::from_value(serde_json::json!({
        "phase": phase,
        "waitStartedAt": "2026-01-01T00:00:00Z",
        "target": { "pvcPrime": "prime-old" },
        "resolved": {
            "resolution": "Snapshot",
            "kopiaSnapshotID": "k1",
            "identity": { "username": "app", "hostname": "ns", "sourcePath": "/pvc/data" },
        },
        "conditions": [{
            "type": "Ready", "status": "True", "reason": reason, "message": "legacy message",
            "lastTransitionTime": "2026-01-01T00:00:00Z",
        }],
    }))
    .expect("legacy status parses")
}

fn claimant(uid: &str) -> k8s_openapi::api::core::v1::PersistentVolumeClaim {
    serde_json::from_value(serde_json::json!({
        "metadata": { "name": "data", "namespace": "ns", "uid": uid },
        "spec": {},
    }))
    .expect("pvc parses")
}

#[test]
fn adopt_legacy_claim_maps_the_whole_legacy_reason_vocabulary() {
    use RestoreClaimPhase as P;
    // The upgrade path: an existing single-PVC populator keeps working with zero
    // spec change. The phase is keyed on the top-level PHASE first, with the
    // Ready reason used only to tell `Completed`'s three situations apart.
    let cases: &[(&str, &str, P)] = &[
        // Completed: a genuine success and a deploy-or-restore are Populated…
        ("Completed", "RestoreSucceeded", P::Populated),
        ("Completed", "NoSnapshotContinue", P::Populated),
        // …the #233 no-op is AlreadyBound…
        ("Completed", "TargetAlreadyBound", P::AlreadyBound),
        // …and the legacy STUCK orphan still needs driving.
        ("Completed", "PopulatingPrimePvc", P::Populating),
        // Failures keep their reason and stay Failed.
        ("Failed", "MoverJobFailed", P::Failed),
        ("Failed", "MoverPodWedged", P::Failed),
        ("Failed", "PopulateHijacked", P::Failed),
        ("Failed", "SnapshotNotFound", P::Failed),
        // In-flight and waiting shapes.
        ("Restoring", "PopulatingPrimePvc", P::Populating),
        ("Restoring", "NoSnapshotContinue", P::Populating),
        ("Pending", "AwaitingPvcDataSourceRef", P::Pending),
        ("Pending", "AwaitingPodSchedule", P::Pending),
        ("Pending", "WaitingForSnapshot", P::Pending),
        ("Pending", "RepositoryNotReady", P::Pending),
        ("Pending", "RestoreReferentMissing", P::Pending),
        ("Resolving", "SourceResolved", P::Pending),
        ("Resolving", "ClaimRecreated", P::Pending),
        // A phase this build cannot read is driven, not settled.
        ("Quiescing", "SourceResolved", P::Pending),
    ];
    for (phase, reason, expected) in cases {
        let adopted = adopt_legacy_claim(&legacy_status(phase, reason), &claimant("u1"), None)
            .unwrap_or_else(|| panic!("{phase}/{reason} must adopt"));
        assert_eq!(
            adopted.phase.as_ref(),
            Some(expected),
            "{phase} + {reason} adopted as {:?}",
            adopted.phase
        );
        // The legacy reason is KEPT verbatim, so a `RestoreSucceeded` legacy is
        // never relabeled `TargetAlreadyBound` (which would tell the user no
        // restore ever ran).
        assert_eq!(adopted.reason.as_deref(), Some(*reason));
        assert_eq!(adopted.message.as_deref(), Some("legacy message"));
        // The pin, the wait anchor, the prime and the path all carry over, so the
        // adopted claim never re-resolves or restarts its window.
        assert_eq!(adopted.uid.as_deref(), Some("u1"));
        assert_eq!(
            adopted.wait_started_at.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(adopted.pvc_prime.as_deref(), Some("prime-old"));
        assert_eq!(adopted.source_path.as_deref(), Some("/pvc/data"));
        assert_eq!(
            adopted
                .resolved
                .and_then(|r| r.kopia_snapshot_id)
                .as_deref(),
            Some("k1")
        );
        // Mover-owned fields are never seeded by a controller-side adoption.
        assert_eq!(adopted.observed_at, None);
        assert_eq!(adopted.log_tail, None);
        assert_eq!(adopted.failure, None);
    }
}

#[test]
fn adopt_legacy_claim_only_fires_on_a_legacy_shaped_status() {
    use RestoreClaimPhase as P;
    // Already fanned out → nothing to adopt (adopting would overwrite live
    // records with the legacy top-level state).
    let mut fanned = legacy_status("Completed", "RestoreSucceeded");
    fanned
        .claims
        .insert("data".into(), claim(Some(P::Populated), None));
    assert_eq!(adopt_legacy_claim(&fanned, &claimant("u1"), None), None);

    // No phase at all now ADOPTS (#443 final review, Important 2). It used to
    // return `None` on the theory that a phase-less status is a brand-new
    // `Restore` — but a brand-new `Restore` no longer reaches this function at
    // all: [`legacy_adoption`] answers `No` for it. The only caller that gets
    // here with no phase is the upgrade window, where the old operator created
    // the prime and the `{restore}-populate` Job and died before its first
    // status patch. Refusing there drives the claim `Fresh` and puts a SECOND
    // mover into that prime.
    let fresh = kopiur_api::RestoreStatus::default();
    let adopted_fresh = adopt_legacy_claim(&fresh, &claimant("u1"), Some("r-populate"))
        .expect("the upgrade window adopts on the prime signal");
    assert_eq!(
        adopted_fresh.phase,
        Some(P::Pending),
        "drive it, do not settle it"
    );
    assert_eq!(adopted_fresh.job.as_deref(), Some("r-populate"));

    // An in-flight legacy populate is driven under ITS OWN Job name, so a second
    // mover is never launched into the same prime PVC.
    let adopted = adopt_legacy_claim(
        &legacy_status("Restoring", "PopulatingPrimePvc"),
        &claimant("u1"),
        Some("r-populate"),
    )
    .expect("adopts");
    assert_eq!(adopted.job.as_deref(), Some("r-populate"));
    // …and when no legacy Job exists, the claim gets a fresh one.
    let adopted = adopt_legacy_claim(
        &legacy_status("Restoring", "PopulatingPrimePvc"),
        &claimant("u1"),
        None,
    )
    .expect("adopts");
    assert_eq!(adopted.job, None);
}

#[test]
fn claim_pinned_decision_keeps_all_four_arms_of_the_restore_level_one() {
    use RestoreClaimPhase as P;
    let with_resolved = |v: serde_json::Value| RestoreClaimStatus {
        resolved: Some(serde_json::from_value(v).expect("resolved parses")),
        ..Default::default()
    };

    // A pinned `NoSnapshot` reads as "deliberately empty" — a snapshot that
    // appeared later must never retarget it.
    assert_eq!(
        claim_pinned_decision(&with_resolved(
            serde_json::json!({ "resolution": "NoSnapshot" })
        )),
        Some(Resolution::Empty)
    );
    // A pinned id wins, with or without the newer `resolution` field (a legacy
    // pin written before it existed still reads).
    for v in [
        serde_json::json!({ "resolution": "Snapshot", "kopiaSnapshotID": "k9" }),
        serde_json::json!({ "kopiaSnapshotID": "k9" }),
    ] {
        assert_eq!(
            claim_pinned_decision(&with_resolved(v)),
            Some(Resolution::Snapshot("k9".into()))
        );
    }
    // A `resolved` block with neither falls through to a fresh resolution.
    assert_eq!(
        claim_pinned_decision(&with_resolved(serde_json::json!({}))),
        None
    );

    // Unpinned + `Populated`: the pre-fix `Continue` arm completed without
    // pinning, so back-fill "empty" rather than re-resolve.
    assert_eq!(
        claim_pinned_decision(&claim(Some(P::Populated), None)),
        Some(Resolution::Empty)
    );
    // Unpinned + `AlreadyBound` is the #233 carve-out: that claim never ran a
    // mover, so back-filling `NoSnapshot` would make a later, legitimate
    // re-creation of the claiming PVC come up EMPTY instead of restoring.
    assert_eq!(
        claim_pinned_decision(&claim(Some(P::AlreadyBound), None)),
        None
    );
    // Every other unpinned state resolves now.
    for p in [
        None,
        Some(P::Pending),
        Some(P::Populating),
        Some(P::Rebinding),
        Some(P::Failed),
        Some(P::Unknown("Quiescing".into())),
    ] {
        assert_eq!(claim_pinned_decision(&claim(p, None)), None);
    }
}

#[test]
fn claim_merge_body_nulls_vanished_controller_keys_and_never_names_mover_keys() {
    use RestoreClaimPhase as P;
    let prev = RestoreClaimStatus {
        uid: Some("u1".into()),
        phase: Some(P::Populating),
        reason: Some("PopulatingPrimePvc".into()),
        message: Some("m".into()),
        source_path: Some("/pvc/data".into()),
        pvc_prime: Some("prime-u1".into()),
        job: Some("r-populate-abc".into()),
        wait_started_at: Some("2026-01-01T00:00:00Z".into()),
        // Mover-owned values already on the object.
        observed_at: Some("2026-01-01T00:05:00Z".into()),
        log_tail: Some("Restore completed".into()),
        ..Default::default()
    };
    // The claim settles: the prime, Job and wait anchor are gone.
    let next = RestoreClaimStatus {
        uid: Some("u1".into()),
        phase: Some(P::Populated),
        reason: Some("RestoreSucceeded".into()),
        message: Some("done".into()),
        source_path: Some("/pvc/data".into()),
        ..Default::default()
    };
    let body = claim_merge_body(Some(&prev), &next);
    assert_eq!(body["phase"], "Populated");
    assert_eq!(body["reason"], "RestoreSucceeded");
    // Explicit nulls: a merge patch removes only the keys it NAMES, and every
    // field is `skip_serializing_if`, so a `None` would leave a reaped prime and
    // a spent anchor standing forever.
    for key in ["pvcPrime", "job", "waitStartedAt"] {
        assert_eq!(body[key], serde_json::Value::Null, "{key} in {body}");
    }
    // Mover-owned keys are NEVER named — not written, not nulled — so a
    // controller pass can never blank what this claim's own mover wrote.
    for key in ["observedAt", "logTail", "failure"] {
        assert!(body.get(key).is_none(), "{key} must be absent: {body}");
    }

    // A brand-new record nulls nothing.
    let body = claim_merge_body(None, &next);
    assert!(
        !body.as_object().unwrap().values().any(|v| v.is_null()),
        "{body}"
    );

    // A controller-pinned `resolved` IS written…
    let pinned = RestoreClaimStatus {
        resolved: Some(
            serde_json::from_value(serde_json::json!({ "kopiaSnapshotID": "k9" }))
                .expect("resolved parses"),
        ),
        ..next.clone()
    };
    assert_eq!(
        claim_merge_body(Some(&prev), &pinned)["resolved"]["kopiaSnapshotID"],
        "k9"
    );
    // …but a pin the controller does not carry this pass is never NULLED: the
    // mover writes `resolved` too, and a claim's pin is durable.
    let prev_pinned = RestoreClaimStatus {
        resolved: Some(
            serde_json::from_value(serde_json::json!({ "kopiaSnapshotID": "k9" }))
                .expect("resolved parses"),
        ),
        ..prev.clone()
    };
    assert!(
        claim_merge_body(Some(&prev_pinned), &next)
            .get("resolved")
            .is_none(),
        "a mover-written pin must survive a controller pass"
    );
}

#[test]
fn claim_transition_fires_only_on_a_real_phase_or_reason_move() {
    use RestoreClaimPhase as P;
    let populating = claim(Some(P::Populating), Some("PopulatingPrimePvc"));
    // A new record is always a transition.
    assert!(claim_transition(None, &populating));
    // The steady-state heartbeat is not — the Event recorder aggregates
    // identical Events into one object, so a per-pass emit would just spin one
    // Event's `count`.
    assert!(!claim_transition(Some(&populating), &populating));
    // A phase move fires.
    assert!(claim_transition(
        Some(&populating),
        &claim(Some(P::Populated), Some("PopulatingPrimePvc"))
    ));
    // …and so does a reason move under the same phase (Pending/AwaitingPodSchedule
    // → Pending/WaitingForSnapshot is real news).
    assert!(claim_transition(
        Some(&claim(Some(P::Pending), Some("AwaitingPodSchedule"))),
        &claim(Some(P::Pending), Some("WaitingForSnapshot"))
    ));
    // An unreadable phase compares by its stored label, so it neither fires
    // spuriously nor hides a move away from it.
    let unknown = claim(Some(P::Unknown("Quiescing".into())), None);
    assert!(!claim_transition(Some(&unknown), &unknown));
    assert!(claim_transition(
        Some(&unknown),
        &claim(Some(P::Pending), None)
    ));
    // A message-only change does NOT fire: messages carry volatile detail and
    // would inflate Events with no news.
    let chatty = RestoreClaimStatus {
        message: Some("different".into()),
        ..populating.clone()
    };
    assert!(!claim_transition(Some(&populating), &chatty));
}

#[test]
fn claim_wait_window_prefers_the_claim_anchor_then_the_legacy_one_then_now() {
    let anchored = RestoreClaimStatus {
        wait_started_at: Some("2026-01-01T00:00:00Z".into()),
        ..Default::default()
    };
    let legacy = restore_with_anchor(Some("2025-06-01T00:00:00Z"));
    let bare = restore_with_anchor(None);
    let now = 1_800_000_000_i64;

    // The claim's own anchor wins: a sibling created an hour later gets its own
    // full window rather than the remains of the first claim's.
    assert_eq!(
        claim_wait_window(Some(&anchored), &legacy, now),
        WaitWindow::Open(1_767_225_600)
    );
    // No claim anchor yet → the legacy top-level one, so an upgrade mid-wait does
    // not silently restart the window.
    assert_eq!(
        claim_wait_window(None, &legacy, now),
        WaitWindow::Open(1_748_736_000)
    );
    // Neither → this pass is the first on which the claim could proceed.
    assert_eq!(claim_wait_window(None, &bare, now), WaitWindow::Open(now));
    assert_eq!(
        claim_wait_window(Some(&RestoreClaimStatus::default()), &bare, now),
        WaitWindow::Open(now)
    );
    // An unparseable anchor falls through rather than panicking.
    let bad = RestoreClaimStatus {
        wait_started_at: Some("yesterday".into()),
        ..Default::default()
    };
    assert_eq!(
        claim_wait_window(Some(&bad), &bare, now),
        WaitWindow::Open(now)
    );
}
