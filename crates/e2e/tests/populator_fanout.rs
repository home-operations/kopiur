//! #443 — a populator `Restore` fills EVERY claiming PVC, each from its own
//! per-PVC kopia source path.
//!
//! Two bugs in one report, and this file guards both:
//!
//! * **Fan-out.** `claiming_pvc` LISTed the namespace and `.find()`-ed the FIRST
//!   claimant, so with a `pvcSelector` policy and N claiming PVCs, claimants
//!   2..N sat `Pending` forever while the `Restore` reported `Completed`.
//! * **Cross-volume.** A `fromPolicy` restore of a selector policy resolved
//!   `source_path: None`, which becomes the kopia filter `user@host:` — an EMPTY
//!   path that matches EVERY member — so the one PVC that did get filled could
//!   be filled with a DIFFERENT volume's data. That one is silent: a green
//!   restore holding the wrong bytes.
//!
//! CSI-conditional (`csi_class_present_or_skip`) on purpose. The harness's
//! static hostPath PVCs all share one source directory, so they physically
//! cannot prove cross-volume isolation — a test over them would pass with the
//! bug still present. The policy uses `groupBy: None` + `copyMethod: Direct`
//! (as `multi_pvc_group.rs` does) so no VolumeGroupSnapshot/VolumeSnapshot CRDs
//! are involved: this needs a populator-aware provisioner, not the snapshot
//! machinery.

#![cfg(all(unix, feature = "e2e"))]

mod common;

use common::*;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kopiur_api::{Restore, Snapshot, SnapshotPolicy, SnapshotSchedule};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};
use kube::ResourceExt;
use kube::api::{Api, DeleteParams, ListParams, PostParams};

/// A THIRD distinct `backup=` label value, alongside `multi_pvc_group.rs`'s
/// `fanout` and `group`.
///
/// The scenarios share one cluster and one namespace and none of them deletes
/// its PVCs, so a shared value would make this selector match volumes belonging
/// to another test — failing on a count assertion that has nothing to do with
/// what this file tests, and only in whichever order nextest happened to pick.
const RESTORE_LABEL_VALUE: &str = "restorefanout";

/// The repo subpath this file owns. Keep in lockstep with `REPO_SUBPATHS`
/// (`kopiur_e2e::consts`) and the `e2e-node-seed` mise task.
const REPO_SUBPATH: &str = "populator-fanout";

const REPO: &str = "e2e-repo-popfanout";
const POLICY: &str = "e2e-popfanout-policy";
const SCHEDULE: &str = "e2e-popfanout-schedule";

/// The two source volumes, with the markers that prove which is which.
const SOURCES: [(&str, &str); 2] = [
    ("e2e-popfan-alpha", "alpha-bytes"),
    ("e2e-popfan-bravo", "bravo-bytes"),
];

/// A `pvcSelector` `SnapshotPolicy` over [`RESTORE_LABEL_VALUE`], with an
/// optional overlay merged into its spec.
fn selector_policy_json(name: &str, strategy: &str, extra: serde_json::Value) -> serde_json::Value {
    snapshot_policy_json(
        E2E_NAMESPACE,
        name,
        "Repository",
        REPO,
        merge_spec(
            serde_json::json!({
                "sources": [ {
                    "pvcSelector": {
                        "labelSelector": {
                            "matchLabels": { BACKUP_LABEL_KEY: RESTORE_LABEL_VALUE }
                        }
                    },
                    "sourcePathStrategy": strategy
                } ],
                // No VolumeGroupSnapshot / VolumeSnapshot machinery: this file is
                // about the populator handshake, not about staging.
                "groupBy": "None",
                "copyMethod": "Direct",
                "identity": { "username": "popfanout", "hostname": "e2e" }
            }),
            extra,
        ),
    )
}

/// The `Snapshot` CRs a policy produced, by its config label.
async fn children_of(client: &kube::Client, policy: &str) -> Vec<Snapshot> {
    let api: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    api.list(&ListParams::default().labels(&format!("kopiur.home-operations.com/config={policy}")))
        .await
        .expect("list Snapshots")
        .items
}

/// Back the two labelled source PVCs up under one selector policy, then DELETE
/// them — the disaster this file recovers from. Leaves the repository holding
/// one kopia source path per volume.
///
/// Idempotent enough to survive a re-run: the repo/policy creates tolerate an
/// existing object, and the source PVCs are re-created each time.
async fn seed_two_member_backups(client: &kube::Client) {
    ensure_repo(client, REPO_SUBPATH).await;
    let repos: Api<kopiur_api::Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    create_idempotent(
        &repos,
        &cr(repository_json(REPO, REPO_SUBPATH, serde_json::json!({}))),
        "create Repository",
    )
    .await;
    wait_phase(&repos, REPO, "Ready")
        .await
        .expect("the fan-out repository must reach Ready");

    for (name, marker) in SOURCES {
        csi_pvc_with_data(client, name, RESTORE_LABEL_VALUE, marker).await;
    }

    create_idempotent(
        &policies,
        &cr(selector_policy_json(
            POLICY,
            "PvcName",
            serde_json::json!({}),
        )),
        "create selector SnapshotPolicy",
    )
    .await;
    // `runOnCreate` fires immediately.
    create_idempotent(
        &schedules,
        &cr(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotSchedule",
            "metadata": { "name": SCHEDULE, "namespace": E2E_NAMESPACE },
            "spec": {
                "policyRef": { "name": POLICY },
                "schedule": { "cron": "0 3 * * *", "runOnCreate": true }
            }
        })),
        "create SnapshotSchedule",
    )
    .await;

    wait_until(
        "two fanned-out source Snapshots",
        default_timeout(),
        poll_interval(),
        || async { Ok((children_of(client, POLICY).await.len() == 2).then_some(())) },
    )
    .await
    .expect("a 2-PVC selector policy must produce exactly 2 Snapshots");

    let mut paths = Vec::new();
    for child in children_of(client, POLICY).await {
        let name = child.name_any();
        wait_phase(&backups, &name, "Succeeded")
            .await
            .unwrap_or_else(|e| panic!("source Snapshot {name} should succeed: {e}"));
        let s = status_json(&backups, &name).await;
        paths.push(
            s["resolved"]["sources"][0]["sourcePath"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        );
    }
    paths.sort();
    paths.dedup();
    assert_eq!(
        paths.len(),
        2,
        "each member must be backed up under its OWN kopia source path, or the restore \
         side has nothing to tell them apart: {paths:?}"
    );

    // Stop the schedule so nothing re-snapshots the restored volumes mid-test,
    // and delete the sources: the restore has to bring them back.
    let _ = schedules.delete(SCHEDULE, &DeleteParams::default()).await;
    for (name, _) in SOURCES {
        let _ = pvcs.delete(name, &DeleteParams::default()).await;
        let _ = pvcs
            .delete(&format!("{name}-seed"), &DeleteParams::default())
            .await;
    }
    for (name, _) in SOURCES {
        wait_until(
            &format!("source PVC {name} is gone"),
            default_timeout(),
            poll_interval(),
            || async { Ok(pvcs.get_opt(name).await?.is_none().then_some(())) },
        )
        .await
        .unwrap_or_else(|e| panic!("source PVC {name} must be deleted before the restore: {e}"));
    }
}

/// A `fromPolicy` populator `Restore` over [`POLICY`], optionally pinning
/// `source.fromPolicy.sourcePath` and/or an explicit `target`.
fn fanout_restore_json(
    name: &str,
    policy: &str,
    source_path: Option<&str>,
    target: serde_json::Value,
) -> serde_json::Value {
    let mut from_policy = serde_json::json!({ "name": policy });
    if let Some(path) = source_path {
        from_policy["sourcePath"] = serde_json::Value::String(path.to_string());
    }
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": REPO },
            "source": { "fromPolicy": from_policy },
            "target": target
        }
    })
}

/// Purge one scenario's leftovers so a crashed previous try cannot poison it.
async fn clear_restore(client: &kube::Client, restore: &str, claims: &[&str]) {
    use k8s_openapi::api::core::v1::Pod;
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    for claim in claims {
        let _ = pods
            .delete(&format!("{claim}-reader"), &DeleteParams::default())
            .await;
        let _ = pvcs.delete(claim, &DeleteParams::default()).await;
    }
    let _ = restores.delete(restore, &DeleteParams::default()).await;
    wait_until(
        &format!("leftovers of {restore} are gone"),
        default_timeout(),
        poll_interval(),
        || async {
            let restore_gone = restores.get_opt(restore).await?.is_none();
            let mut claims_gone = true;
            for claim in claims {
                claims_gone &= pvcs.get_opt(claim).await?.is_none();
            }
            Ok((restore_gone && claims_gone).then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("a previous try's {restore} must clear first: {e}"));
}

/// `status.claims` as a plain map, for per-claim assertions.
async fn claims_of(
    client: &kube::Client,
    restore: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    status_json(&restores, restore)
        .await
        .get("claims")
        .and_then(|c| c.as_object())
        .cloned()
        .unwrap_or_default()
}

/// **Test 1 — the reported bug.** Two labelled CSI PVCs backed up under one
/// `pvcSelector` policy, both deleted, then ONE populator `Restore` re-creates
/// BOTH — each carrying ITS OWN marker.
///
/// Fails on the pre-#443 code twice over: the second claim never binds (its
/// reader pod never schedules, so `populate_claims` times out), and the first
/// claim can come back holding the other volume's bytes (its reader's `test`
/// fails).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test) + CSI snapshot stack"]
async fn one_populator_restore_fills_every_claiming_pvc_with_its_own_data() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }

    let restore = "e2e-popfan-restore";
    let claims: Vec<&str> = SOURCES.iter().map(|(name, _)| *name).collect();
    clear_restore(&client, restore, &claims).await;
    seed_two_member_backups(&client).await;

    // ONE Restore, TWO claiming PVCs. Each reader asserts its OWN marker — a
    // claim filled from a sibling's snapshot fails here, which is the whole
    // point of the per-PVC source path.
    let specs: Vec<ClaimSpec<'_>> = SOURCES
        .iter()
        .map(|(name, marker)| ClaimSpec {
            name,
            path: "marker.txt",
            expect: marker,
        })
        .collect();
    let populated = populate_claims(
        &client,
        CSI_STORAGE_CLASS,
        restore,
        fanout_restore_json(
            restore,
            POLICY,
            None,
            serde_json::json!({ "populator": {} }),
        ),
        &specs,
    )
    .await;

    // The per-claim record is the user-visible surface of the fan-out.
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let claims_map = claims_of(&client, restore).await;
    assert_eq!(
        claims_map.len(),
        2,
        "status.claims must carry one entry per claiming PVC: {claims_map:#?}"
    );
    for (name, marker) in SOURCES {
        let record = claims_map
            .get(name)
            .unwrap_or_else(|| panic!("status.claims.{name} missing: {claims_map:#?}"));
        assert_eq!(
            record.get("phase").and_then(|p| p.as_str()),
            Some("Populated"),
            "claim {name} must report Populated: {record:#}"
        );
        // The audit trail: each claim records the member path it read, so a
        // cross-volume restore is visible in `kubectl get restore -o yaml` and
        // not only in the bytes. `{marker}` is only here to name the volume in
        // the failure message.
        let path = record
            .get("sourcePath")
            .and_then(|p| p.as_str())
            .unwrap_or_default();
        assert!(
            path.ends_with(name),
            "claim {name} (marker {marker}) must record its OWN source path, got {path:?}"
        );
    }

    let ready = wait_condition(&restores, restore, "Ready", "True")
        .await
        .expect("a fully populated fan-out Restore must reach Ready=True");
    assert_eq!(
        ready.get("reason").and_then(|r| r.as_str()),
        Some("RestoreSucceeded"),
        "{ready:#}"
    );
    // `AwaitingClaim` must be cleared once claims exist — a standing `True`
    // there is how the pre-fix code left a half-done fan-out looking blocked.
    let awaiting = wait_condition(&restores, restore, "AwaitingClaim", "False")
        .await
        .expect("AwaitingClaim must flip False once a PVC claims the Restore");
    assert_eq!(
        awaiting.get("reason").and_then(|r| r.as_str()),
        Some("ClaimsObserved"),
        "{awaiting:#}"
    );

    populated.cleanup(&client).await;
}

/// **Test 2 — direct `target.pvc` against a selector policy.** The same
/// cross-volume hazard by a different route: without an override the restore
/// must derive the path from the TARGET's name, and with
/// `fromPolicy.sourcePath` pinned it must read exactly that member.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test) + CSI snapshot stack"]
async fn a_direct_target_derives_its_member_path_and_honors_the_override() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }
    seed_two_member_backups(&client).await;

    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let (alpha, alpha_marker) = SOURCES[0];
    let (bravo, bravo_marker) = SOURCES[1];

    // (a) No override: the target's own name selects its own member.
    //     `target.pvc` names the PVC the operator creates, so it is named after
    //     the member it restores.
    let derived = "e2e-popfan-direct";
    let _ = restores.delete(derived, &DeleteParams::default()).await;
    restores
        .create(
            &PostParams::default(),
            &cr(fanout_restore_json(
                derived,
                POLICY,
                None,
                serde_json::json!({ "pvc": {
                    "name": alpha,
                    "storageClassName": CSI_STORAGE_CLASS,
                    "capacity": "64Mi",
                    "accessModes": ["ReadWriteOnce"]
                } }),
            )),
        )
        .await
        .expect("create the derived-path direct Restore");
    wait_phase(&restores, derived, "Completed")
        .await
        .expect("a direct restore against a selector policy must complete");
    assert_marker(&client, &format!("{derived}-reader"), alpha, alpha_marker).await;

    // (b) With `fromPolicy.sourcePath` pinned to the OTHER member, the same
    //     target comes back holding bravo's bytes — proving the override, not
    //     the target name, is what selected the member.
    let overridden = "e2e-popfan-override";
    let overridden_pvc = "e2e-popfan-override-data";
    let _ = restores.delete(overridden, &DeleteParams::default()).await;
    restores
        .create(
            &PostParams::default(),
            &cr(fanout_restore_json(
                overridden,
                POLICY,
                Some(&format!("/pvc/{bravo}")),
                serde_json::json!({ "pvc": {
                    "name": overridden_pvc,
                    "storageClassName": CSI_STORAGE_CLASS,
                    "capacity": "64Mi",
                    "accessModes": ["ReadWriteOnce"]
                } }),
            )),
        )
        .await
        .expect("create the overridden-path direct Restore");
    wait_phase(&restores, overridden, "Completed")
        .await
        .expect("an explicit fromPolicy.sourcePath must resolve");
    assert_marker(
        &client,
        &format!("{overridden}-reader"),
        overridden_pvc,
        bravo_marker,
    )
    .await;

    let _ = restores.delete(derived, &DeleteParams::default()).await;
    let _ = restores.delete(overridden, &DeleteParams::default()).await;
}

/// Run a one-shot pod asserting `claim`'s `marker.txt` holds `expect`.
async fn assert_marker(client: &kube::Client, reader: &str, claim: &str, expect: &str) {
    use k8s_openapi::api::core::v1::Pod;
    use kopiur_e2e::{builders, wait};
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = pods.delete(reader, &DeleteParams::default()).await;
    let script = format!("test \"$(cat /mnt/marker.txt)\" = '{expect}'");
    pods.create(
        &PostParams::default(),
        &builders::one_shot_pod(
            E2E_NAMESPACE,
            reader,
            &["sh", "-c", &script],
            &[(claim, "/mnt")],
        ),
    )
    .await
    .expect("create reader pod");
    wait::pod_succeeded(client, E2E_NAMESPACE, reader)
        .await
        .unwrap_or_else(|e| panic!("{claim} must hold marker {expect:?}: {e}"));
    let _ = pods.delete(reader, &DeleteParams::default()).await;
}

/// **Test 3 — fail closed.** A policy whose two selector sources disagree on
/// `sourcePathStrategy` names no single volume for a target, so the claim must
/// FAIL with `SourcePathAmbiguous` and the `Restore` must go `Stalled`. Filling
/// it from a pathless identity — the pre-#443 behavior — would silently restore
/// whichever member kopia listed newest.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test) + CSI snapshot stack"]
async fn an_ambiguous_policy_fails_the_claim_closed_and_names_the_fix() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }
    seed_two_member_backups(&client).await;

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let ambiguous = "e2e-popfan-ambiguous-policy";
    let restore = "e2e-popfan-ambiguous";
    let claim = "e2e-popfan-ambiguous-data";
    clear_restore(&client, restore, &[claim]).await;
    let _ = policies.delete(ambiguous, &DeleteParams::default()).await;

    // TWO selector sources that disagree on the strategy: no derivation can say
    // which member a given PVC's data lives under.
    create_idempotent(
        &policies,
        &cr(selector_policy_json(
            ambiguous,
            "PvcName",
            serde_json::json!({
                "sources": [
                    { "pvcSelector": { "labelSelector": {
                        "matchLabels": { BACKUP_LABEL_KEY: RESTORE_LABEL_VALUE } } },
                      "sourcePathStrategy": "PvcName" },
                    { "pvcSelector": { "labelSelector": {
                        "matchLabels": { BACKUP_LABEL_KEY: RESTORE_LABEL_VALUE } } },
                      "sourcePathStrategy": "PvcNamespacedName" }
                ]
            }),
        )),
        "create the ambiguous selector SnapshotPolicy",
    )
    .await;

    restores
        .create(
            &PostParams::default(),
            &cr(fanout_restore_json(
                restore,
                ambiguous,
                None,
                serde_json::json!({ "populator": {} }),
            )),
        )
        .await
        .expect("create the ambiguous populator Restore");
    pvcs.create(
        &PostParams::default(),
        &cr(claiming_pvc_json(claim, CSI_STORAGE_CLASS, restore)),
    )
    .await
    .expect("create claiming PVC");

    wait_until(
        "the ambiguous claim fails closed",
        default_timeout(),
        poll_interval(),
        || async {
            let record = claims_of(&client, restore).await.get(claim).cloned();
            Ok(record.filter(|r| r.get("phase").and_then(|p| p.as_str()) == Some("Failed")))
        },
    )
    .await
    .map(|record| {
        assert_eq!(
            record.get("reason").and_then(|r| r.as_str()),
            Some("SourcePathAmbiguous"),
            "{record:#}"
        );
        let message = record
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default();
        assert!(
            message.contains("fromPolicy.sourcePath"),
            "the message must name the field that fixes it: {message}"
        );
    })
    .expect("an ambiguous policy must FAIL the claim, never guess a member path");

    // The Restore stalls (kstatus), so `kubectl wait`/Flux see the failure
    // rather than a restore that quietly never lands.
    wait_condition(&restores, restore, "Stalled", "True")
        .await
        .expect("a failed claim must stall the Restore");

    let _ = restores.delete(restore, &DeleteParams::default()).await;
    let _ = pvcs.delete(claim, &DeleteParams::default()).await;
    let _ = policies.delete(ambiguous, &DeleteParams::default()).await;
}

/// **Test 4 — sibling isolation and re-arm.** One claim fails (its
/// `fromPolicy.sourcePath` names a member that does not exist, under
/// `onMissingSnapshot: Fail`) while its sibling still binds; deleting and
/// re-creating the failed PVC re-arms that claim.
///
/// This is the behavior the `phase_is_terminal_at_guard` relaxation exists for:
/// before #443 a `Failed` populator short-circuited the whole reconcile, so the
/// sibling would never have been driven at all.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test) + CSI snapshot stack"]
async fn a_failed_claim_stalls_the_restore_without_stopping_its_siblings() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }
    seed_two_member_backups(&client).await;

    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let (alpha, alpha_marker) = SOURCES[0];
    let restore = "e2e-popfan-sibling";
    // The good claim is named after a real member, so the derivation finds it;
    // the bad one is named after a member that was never backed up.
    let good = alpha;
    let bad = "e2e-popfan-missing";
    clear_restore(&client, restore, &[good, bad]).await;

    restores
        .create(
            &PostParams::default(),
            &cr({
                let mut json = fanout_restore_json(
                    restore,
                    POLICY,
                    None,
                    serde_json::json!({ "populator": {} }),
                );
                // Fail closed on a missing snapshot rather than provisioning an
                // empty volume, and give the wait window no room, so the bad
                // claim reaches its terminal state inside the test's budget.
                json["spec"]["policy"] =
                    serde_json::json!({ "onMissingSnapshot": "Fail", "waitTimeout": "10s" });
                json
            }),
        )
        .await
        .expect("create the sibling-isolation Restore");
    for claim in [good, bad] {
        pvcs.create(
            &PostParams::default(),
            &cr(claiming_pvc_json(claim, CSI_STORAGE_CLASS, restore)),
        )
        .await
        .unwrap_or_else(|e| panic!("create claiming PVC {claim}: {e}"));
    }

    // The GOOD sibling binds and carries its own data even though its sibling
    // is doomed — the regression guard for the guard relaxation.
    assert_marker(&client, &format!("{good}-reader"), good, alpha_marker).await;

    wait_until(
        "the bad claim reaches Failed while its sibling is Populated",
        default_timeout(),
        poll_interval(),
        || async {
            let claims = claims_of(&client, restore).await;
            let bad_failed = claims
                .get(bad)
                .and_then(|r| r.get("phase"))
                .and_then(|p| p.as_str())
                == Some("Failed");
            let good_ok = claims
                .get(good)
                .and_then(|r| r.get("phase"))
                .and_then(|p| p.as_str())
                == Some("Populated");
            Ok((bad_failed && good_ok).then_some(()))
        },
    )
    .await
    .expect("one failed claim must not stop its sibling from being populated");

    // Re-arm: deleting and re-creating the claiming PVC mints a new uid, which
    // is what drops the failed record and starts the claim over.
    let _ = pvcs.delete(bad, &DeleteParams::default()).await;
    wait_until(
        "the failed claim's record is dropped with its PVC",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(claims_of(&client, restore)
                .await
                .get(bad)
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("a claim record must not outlive its claimant");
    pvcs.create(
        &PostParams::default(),
        &cr(claiming_pvc_json(bad, CSI_STORAGE_CLASS, restore)),
    )
    .await
    .expect("re-create the failed claiming PVC");
    wait_until(
        "the re-created claim is driven again",
        default_timeout(),
        poll_interval(),
        || async { Ok(claims_of(&client, restore).await.get(bad).cloned()) },
    )
    .await
    .expect("re-creating the claiming PVC must re-arm the claim");

    let _ = restores.delete(restore, &DeleteParams::default()).await;
    for claim in [good, bad] {
        let _ = pvcs.delete(claim, &DeleteParams::default()).await;
    }
}
