//! Unit tests for VolumeGroupSnapshot staging.
//!
//! Everything decision-shaped here is pure: readiness (which must not fail on a
//! transient error), the three-hop member→PVC reconstruction `v1beta1` forces,
//! and the reap gate that must fail closed.

use super::*;
use chrono::{TimeZone, Utc};

fn t(h: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 5, h, 0, 0).unwrap()
}

fn obs(ready: bool, error: Option<&str>, created: Option<u32>) -> VgsObservation {
    VgsObservation {
        ready,
        error: error.map(str::to_string),
        created_at: created.map(t),
    }
}

const TEN_MIN: std::time::Duration = std::time::Duration::from_secs(600);

// --- readiness --------------------------------------------------------------

#[test]
fn ready_wins_over_both_the_error_and_the_deadline() {
    // Order matters: a group that IS usable must never be failed because a
    // stale error field lingered or the clock passed the deadline.
    let o = obs(true, Some("some earlier hiccup"), Some(1));
    assert_eq!(
        vgs_wait_outcome(&o, "ns", "g", "cls", Some(TEN_MIN), t(23)),
        VgsWait::Ready
    );
}

#[test]
fn an_error_is_never_fatal_on_sight() {
    // #198, carried over from single-PVC staging: the snapshot-controller sets
    // `status.error` transiently during benign retries (a 409 finalizer-add
    // conflict) and clears it on the next successful sync. Failing here would
    // make a group flap into a TERMINAL state for N Snapshots at once.
    let o = obs(
        false,
        Some("failed to create group snapshot: conflict"),
        Some(1),
    );
    match vgs_wait_outcome(&o, "ns", "g", "cls", Some(TEN_MIN), t(1)) {
        VgsWait::Waiting(msg) => {
            assert!(msg.contains("possibly-transient"), "{msg}");
            assert!(
                msg.contains("conflict"),
                "the error is still surfaced: {msg}"
            );
        }
        other => panic!("an error inside the deadline must WAIT, got {other:?}"),
    }
}

#[test]
fn the_deadline_is_what_fails_and_it_names_the_group_escape_hatch() {
    let o = obs(false, None, Some(1));
    match vgs_wait_outcome(&o, "ns", "g", "cls", Some(TEN_MIN), t(3)) {
        VgsWait::Failed { reason, message } => {
            assert_eq!(reason, crate::consts::REASON_GROUP_STAGING_TIMEOUT);
            assert!(message.contains("10m"), "must echo the budget: {message}");
            assert!(
                message.contains("groupBy: None"),
                "must name the way out: {message}"
            );
        }
        other => panic!("past the deadline must fail, got {other:?}"),
    }
    // With an error at the deadline, the reason distinguishes the two causes.
    let o = obs(false, Some("driver said no"), Some(1));
    match vgs_wait_outcome(&o, "ns", "g", "cls", Some(TEN_MIN), t(3)) {
        VgsWait::Failed { reason, message } => {
            assert_eq!(reason, crate::consts::REASON_VGS_FAILED);
            assert!(message.contains("driver said no"), "{message}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn a_missing_creation_timestamp_or_zero_timeout_never_expires() {
    // A just-created object whose metadata has not landed yet must not be
    // instantly failed; `spec.staging.timeout: 0` means wait forever.
    assert!(matches!(
        vgs_wait_outcome(
            &obs(false, None, None),
            "ns",
            "g",
            "c",
            Some(TEN_MIN),
            t(23)
        ),
        VgsWait::Waiting(_)
    ));
    match vgs_wait_outcome(&obs(false, None, Some(1)), "ns", "g", "c", None, t(23)) {
        VgsWait::Waiting(msg) => assert!(msg.contains("indefinitely"), "{msg}"),
        other => panic!("expected Waiting, got {other:?}"),
    }
}

#[test]
fn the_waiting_message_is_deterministic_so_status_does_not_churn() {
    // All N members compute the identical deadline from the GROUP's own
    // creationTimestamp, so they emit byte-identical messages with no
    // coordination — and `patch_status_if_changed` stays a no-op in steady state.
    let o = obs(false, None, Some(1));
    let a = vgs_wait_outcome(&o, "ns", "g", "c", Some(TEN_MIN), t(1));
    let b = vgs_wait_outcome(&o, "ns", "g", "c", Some(TEN_MIN), t(1));
    assert_eq!(a, b);
    // ...and it does not embed `now`, so a later reconcile inside the deadline
    // still produces the same string.
    assert_eq!(a, vgs_wait_outcome(&o, "ns", "g", "c", Some(TEN_MIN), t(1)));
}

// --- member mapping ---------------------------------------------------------

fn member(name: &str, group: &str, content: &str) -> MemberVs {
    MemberVs {
        name: name.to_string(),
        group: Some(group.to_string()),
        content: Some(content.to_string()),
        restore_size: Some(Quantity("1Gi".into())),
    }
}

#[test]
fn members_map_back_to_their_pvcs_through_content_and_pv() {
    // v1beta1's group status carries NO member list, so this three-hop
    // reconstruction is the only way: member VS -> its content -> the CSI
    // volumeHandle -> the PV with that handle -> its claimRef.
    let members = vec![
        member("snap-a", "grp", "content-a"),
        member("snap-b", "grp", "content-b"),
    ];
    let contents = vec![
        ContentInfo {
            name: "content-a".into(),
            volume_handle: Some("vol-1".into()),
        },
        ContentInfo {
            name: "content-b".into(),
            volume_handle: Some("vol-2".into()),
        },
    ];
    let pvs = vec![
        PvInfo {
            volume_handle: "vol-1".into(),
            claim_namespace: "db".into(),
            claim_name: "pgdata".into(),
        },
        PvInfo {
            volume_handle: "vol-2".into(),
            claim_namespace: "db".into(),
            claim_name: "wal".into(),
        },
    ];
    let map = map_group_members(&members, &contents, &pvs, "grp");
    assert_eq!(map.len(), 2);
    assert_eq!(
        map[&("db".to_string(), "pgdata".to_string())].volume_snapshot_name,
        "snap-a"
    );
    assert_eq!(
        map[&("db".to_string(), "wal".to_string())].volume_snapshot_name,
        "snap-b"
    );
}

#[test]
fn snapshots_belonging_to_another_group_are_ignored() {
    // Members are listed namespace-wide, so a concurrent expansion's snapshots
    // are in the same list. Mapping one of those to this group's PVC would
    // stage a backup from the WRONG point in time.
    let mut other = member("snap-other", "some-other-group", "content-a");
    other.group = Some("other-grp".into());
    let contents = vec![ContentInfo {
        name: "content-a".into(),
        volume_handle: Some("vol-1".into()),
    }];
    let pvs = vec![PvInfo {
        volume_handle: "vol-1".into(),
        claim_namespace: "db".into(),
        claim_name: "pgdata".into(),
    }];
    assert!(map_group_members(&[other], &contents, &pvs, "grp").is_empty());
}

#[test]
fn an_unresolvable_member_is_omitted_never_guessed() {
    // Missing content, missing PV, or an unbound snapshot: each omits that PVC.
    // The caller turns a missing SELF entry into a named terminal failure —
    // guessing a neighbour's snapshot would back up the wrong volume.
    let members = vec![
        member("snap-a", "grp", "missing-content"),
        MemberVs {
            name: "snap-b".into(),
            group: Some("grp".into()),
            content: None,
            restore_size: None,
        },
    ];
    let contents = vec![ContentInfo {
        name: "content-x".into(),
        volume_handle: Some("vol-9".into()),
    }];
    let pvs = vec![PvInfo {
        volume_handle: "vol-9".into(),
        claim_namespace: "db".into(),
        claim_name: "x".into(),
    }];
    assert!(map_group_members(&members, &contents, &pvs, "grp").is_empty());
}

#[test]
fn a_content_with_no_volume_handle_is_omitted() {
    let members = vec![member("snap-a", "grp", "content-a")];
    let contents = vec![ContentInfo {
        name: "content-a".into(),
        volume_handle: None,
    }];
    assert!(map_group_members(&members, &contents, &[], "grp").is_empty());
}

// --- reaping ----------------------------------------------------------------

fn sibling(phase: Option<SnapshotPhase>, staged_ready: Option<bool>) -> Snapshot {
    let mut s: Snapshot = serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": "m", "namespace": "ns" },
        "spec": {},
    }))
    .expect("valid Snapshot");
    let mut status = kopiur_api::SnapshotStatus {
        phase,
        ..Default::default()
    };
    if let Some(ready) = staged_ready {
        status.staged = Some(kopiur_api::StagedSources {
            ready: Some(ready),
            ..Default::default()
        });
    }
    s.status = Some(status);
    s
}

#[test]
fn the_group_is_reaped_only_when_every_sibling_is_done() {
    let done = || sibling(Some(SnapshotPhase::Succeeded), None);
    assert!(group_reapable(Some(&[done(), done()])));

    // One still running: the group's member snapshots are what its staged PVC
    // is restoring from, so deleting the group now pulls the rug out.
    assert!(!group_reapable(Some(&[
        done(),
        sibling(Some(SnapshotPhase::Running), None)
    ])));
    assert!(!group_reapable(Some(&[done(), sibling(None, None)])));
}

#[test]
fn a_terminal_sibling_that_still_holds_a_live_stage_blocks_the_reap() {
    // #103's shape: terminal phase does not mean the staged PVC is gone. If it
    // is still up, it may still be restoring from a member snapshot.
    assert!(!group_reapable(Some(&[sibling(
        Some(SnapshotPhase::Succeeded),
        Some(true)
    )])));
    // A torn-down stage does not block.
    assert!(group_reapable(Some(&[sibling(
        Some(SnapshotPhase::Succeeded),
        Some(false)
    )])));
}

#[test]
fn every_terminal_phase_counts_as_done_including_unchanged() {
    for phase in [
        SnapshotPhase::Succeeded,
        SnapshotPhase::Failed,
        SnapshotPhase::Unchanged,
    ] {
        assert!(
            group_reapable(Some(&[sibling(Some(phase), None)])),
            "{phase:?} is terminal and must not pin the group forever"
        );
    }
}

#[test]
fn a_failed_sibling_read_fails_closed() {
    // THE safety property. `None` means "could not list". Treating that as
    // "nobody needs it" would delete a capture N live backups restore from,
    // and the failure would be invisible until a restore came up empty.
    assert!(!group_reapable(None));
    // An empty list is a real answer, not a failed read: nothing needs it.
    assert!(group_reapable(Some(&[])));
}
