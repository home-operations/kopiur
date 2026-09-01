//! #346 — `pvcSelector` fan-out, and `groupBy: VolumeGroupSnapshot` staging.
//!
//! The reported symptom was `invariant violated: backup mover path requires
//! exactly one of source.pvc or source.nfs. This is likely a bug in kopiur` for
//! a config copied almost verbatim from `deploy/examples/04-multi-pvc-selector.yaml`.
//! `pvcSelector` had no implementation at all, so this covers the whole
//! pipeline: expansion → N Snapshot CRs → N mover Jobs → N kopia sources.
//!
//! Two scenarios, deliberately split by what they need:
//!
//! * fan-out with `groupBy: None` over plain hostPath PVCs — no CSI, so it runs
//!   wherever the harness does and guards the reported bug directly;
//! * group staging with `groupBy: VolumeGroupSnapshot` — needs the CSI stack
//!   *and* the `groupsnapshot.storage.k8s.io` CRDs plus the
//!   `--feature-gates=CSIVolumeGroupSnapshot=true` the vendored manifests now
//!   set on both the snapshot-controller and the csi-snapshotter sidecar.

#![cfg(all(unix, feature = "e2e"))]

mod common;

use common::*;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};

/// The storage class the `snapshot-stack` mise task installs.
const CSI_STORAGE_CLASS: &str = "csi-hostpath-sc";

/// The label KEY a `pvcSelector` matches on, mirroring example 04.
const BACKUP_LABEL_KEY: &str = "backup";

/// The two scenarios below use DIFFERENT label values on purpose.
///
/// They share one cluster (one shard, one namespace) and neither deletes its
/// PVCs — the group scenario's `e2e-grp-*` outlive it. A shared value would make
/// the fan-out scenario's selector match four PVCs instead of two, so it would
/// fail on a count assertion that has nothing to do with what it tests, and only
/// in whichever order nextest happened to pick. Distinct values also mean a
/// crashed run cannot poison its sibling.
const FANOUT_LABEL_VALUE: &str = "fanout";
/// See [`FANOUT_LABEL_VALUE`].
const GROUP_LABEL_VALUE: &str = "group";

fn volume_group_snapshots(client: &kube::Client) -> Api<DynamicObject> {
    let ar = ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(
            "groupsnapshot.storage.k8s.io",
            "v1beta1",
            "VolumeGroupSnapshot",
        ),
        "volumegroupsnapshots",
    );
    Api::namespaced_with(client.clone(), E2E_NAMESPACE, &ar)
}

/// Create a CSI PVC carrying the selector label, and seed it so it binds.
async fn csi_pvc_with_data(client: &kube::Client, name: &str, marker: &str) {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = pvcs
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                "metadata": {
                    "name": name, "namespace": E2E_NAMESPACE,
                    "labels": { BACKUP_LABEL_KEY: GROUP_LABEL_VALUE },
                },
                "spec": {
                    "accessModes": ["ReadWriteOnce"],
                    "storageClassName": CSI_STORAGE_CLASS,
                    "resources": { "requests": { "storage": "64Mi" } },
                },
            })),
        )
        .await;
    let _ = pods
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "v1", "kind": "Pod",
                "metadata": { "name": format!("{name}-seed"), "namespace": E2E_NAMESPACE },
                "spec": {
                    "restartPolicy": "Never",
                    "containers": [{
                        "name": "seed", "image": kopiur_e2e::consts::BUSYBOX_IMAGE,
                        "imagePullPolicy": "IfNotPresent",
                        "command": ["sh", "-c", format!("echo {marker} > /data/marker.txt")],
                        "volumeMounts": [{ "name": "d", "mountPath": "/data" }],
                    }],
                    "volumes": [{ "name": "d", "persistentVolumeClaim": { "claimName": name } }],
                },
            })),
        )
        .await;
    wait_until(
        &format!("PVC {name} Bound"),
        default_timeout(),
        poll_interval(),
        || async {
            let bound = pvcs
                .get_opt(name)
                .await?
                .and_then(|p| p.status.and_then(|s| s.phase))
                .as_deref()
                == Some("Bound");
            Ok(bound.then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("PVC {name} should bind: {e}"));
}

/// Fail fast if the created policy is not actually a selector policy.
///
/// The whole test is about fan-out, so a policy that quietly kept the harness's
/// default single-`pvc` source produces exactly ONE child and reports "fan-out
/// is broken" — with nothing anywhere naming the real fault, which is that the
/// selector never reached the CR. Cheaper to catch here than after a timeout.
async fn assert_selector_landed(api: &Api<kopiur_api::SnapshotPolicy>, name: &str) {
    let p = api.get(name).await.expect("read back the policy");
    assert!(
        p.spec.sources.iter().any(|s| s.pvc_selector.is_some()),
        "the created policy must carry a pvcSelector source, got {:?}",
        p.spec.sources
    );
}

/// The `Snapshot` CRs a policy produced, by its config label.
async fn children_of(client: &kube::Client, policy: &str) -> Vec<kopiur_api::Snapshot> {
    let api: Api<kopiur_api::Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    api.list(&ListParams::default().labels(&format!("kopiur.home-operations.com/config={policy}")))
        .await
        .expect("list Snapshots")
        .items
}

/// Purge one scenario's leftovers from a previous try, so the e2e profile's
/// nextest retries actually RE-RUN the scenario instead of dying in setup.
///
/// A panicked try skips the end-of-test cleanup and leaves three tripwires: the
/// policy (the fresh `create` dies `AlreadyExists` — how a real CSI group-member
/// flake turned into 3/3 shard failures on PR #417's merge queue), the schedule
/// (its `runOnCreate` token is consumed, so even an idempotent create fires no
/// new capture), and stale children (a terminal `Failed` member makes the
/// all-Succeeded wait unwinnable). Deletion order mirrors the tests' own
/// success-path cleanup — schedule first so nothing re-produces children — and
/// then waits for the children to fully go (their finalizers release the
/// kopia-side state through the batched delete path). A fresh cluster is a
/// fast no-op.
async fn clear_scenario_leftovers(client: &kube::Client, schedule: &str, policy: &str) {
    let schedules: Api<kopiur_api::SnapshotSchedule> =
        Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<kopiur_api::SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<kopiur_api::Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = schedules.delete(schedule, &DeleteParams::default()).await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    for child in children_of(client, policy).await {
        if let Some(n) = child.metadata.name {
            let _ = backups.delete(&n, &DeleteParams::default()).await;
        }
    }
    wait_until(
        &format!("leftovers of scenario `{policy}` are gone"),
        default_timeout(),
        poll_interval(),
        || async {
            let gone = schedules.get_opt(schedule).await?.is_none()
                && policies.get_opt(policy).await?.is_none()
                && children_of(client, policy).await.is_empty();
            Ok(gone.then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("previous try's `{policy}` leftovers must clear: {e}"));
}

/// A `pvcSelector` policy fires one Snapshot per matched PVC, each backing up
/// its OWN volume at its OWN kopia source path.
///
/// This is #346's headline. `groupBy: None` and `copyMethod: Direct` keep it
/// free of CSI so it guards the reported bug everywhere the harness runs.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn a_pvc_selector_fans_out_to_one_snapshot_per_matched_pvc() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_repo(&client, "multipvc-fanout").await;

    // Two labelled hostPath-backed PVCs. `Direct` reads them live, which is all
    // the fan-out needs to prove.
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    for (pv, pvc) in [
        ("e2e-fanout-pv-a", "e2e-fanout-a"),
        ("e2e-fanout-pv-b", "e2e-fanout-b"),
    ] {
        use kopiur_e2e::apply::{Fixture, apply_all};
        use kopiur_e2e::builders;
        let fixtures: Vec<Fixture> = vec![
            builders::hostpath_pv(pv, kopiur_e2e::consts::HOSTPATH_SRC, "1Gi").into(),
            builders::static_pvc(E2E_NAMESPACE, pvc, pv, "1Gi").into(),
        ];
        apply_all(&client, &fixtures).await.expect("fan-out PVCs");
        // The selector matches on this label; the builder does not set it.
        let patch = serde_json::json!({
            "metadata": { "labels": { BACKUP_LABEL_KEY: FANOUT_LABEL_VALUE } }
        });
        // A plain merge patch, NOT `PatchParams::apply(..).force()`: kube
        // rejects `force` on anything but `Patch::Apply`
        // ("PatchParams::force only works with Patch::Apply"), and adding one
        // label to an object this test already owns needs no field manager.
        pvcs.patch(pvc, &Default::default(), &kube::api::Patch::Merge(&patch))
            .await
            .expect("label the PVC");
    }

    let repos: Api<kopiur_api::Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<kopiur_api::SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<kopiur_api::SnapshotSchedule> =
        Api::namespaced(client.clone(), E2E_NAMESPACE);
    clear_scenario_leftovers(&client, "e2e-fanout-schedule", "e2e-fanout-policy").await;

    let repo = "e2e-repo-multipvc-fanout";
    let _ = repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                repo,
                "multipvc-fanout",
                serde_json::json!({}),
            )),
        )
        .await;
    wait_phase(&repos, repo, "Ready").await.expect("repo Ready");

    let policy = "e2e-fanout-policy";
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                policy,
                "Repository",
                repo,
                serde_json::json!({
                    // The shape from deploy/examples/04-multi-pvc-selector.yaml,
                    // which is what the reporter had.
                    "sources": [ {
                        "pvcSelector": {
                            "labelSelector": { "matchLabels": { BACKUP_LABEL_KEY: FANOUT_LABEL_VALUE } }
                        },
                        "sourcePathStrategy": "PvcName"
                    } ],
                    "groupBy": "None",
                    "copyMethod": "Direct",
                    "identity": { "username": "fanout", "hostname": "e2e" }
                }),
            )),
        )
        .await
        .expect("a pvcSelector policy must be ADMITTED — it is a documented feature");
    assert_selector_landed(&policies, policy).await;

    // `runOnCreate` fires immediately, which is exactly how the reporter hit it.
    let schedule = "e2e-fanout-schedule";
    create_idempotent(
        &schedules,
        &cr(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotSchedule",
            "metadata": { "name": schedule, "namespace": E2E_NAMESPACE },
            "spec": {
                "policyRef": { "name": policy },
                "schedule": { "cron": "0 3 * * *", "runOnCreate": true }
            }
        })),
        "create SnapshotSchedule",
    )
    .await;

    // TWO children, not one, and not an invariant violation.
    wait_until(
        "two fanned-out Snapshots",
        default_timeout(),
        poll_interval(),
        || async {
            let n = children_of(&client, policy).await.len();
            Ok((n == 2).then_some(()))
        },
    )
    .await
    .expect("a 2-PVC selector must produce exactly 2 Snapshots");

    let backups: Api<kopiur_api::Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let mut paths = Vec::new();
    for child in children_of(&client, policy).await {
        let name = child.metadata.name.clone().expect("named");
        wait_phase(&backups, &name, "Succeeded")
            .await
            .unwrap_or_else(|e| panic!("fanned-out Snapshot {name} should succeed: {e}"));

        // Each child pins the PVC it covers — the field whose absence WAS the bug.
        let pinned = child
            .spec
            .source
            .as_ref()
            .map(|s| match &s.target {
                kopiur_api::SnapshotSourceTarget::Pvc(t) => t.name.clone(),
            })
            .unwrap_or_else(|| panic!("child {name} must pin its source PVC"));
        assert!(
            pinned.starts_with("e2e-fanout-"),
            "unexpected pinned PVC {pinned}"
        );
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
        "each child must back up its OWN kopia source path — a shared path merges two \
         volumes' histories into one stream: {paths:?}"
    );

    let _ = schedules.delete(schedule, &DeleteParams::default()).await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    for child in children_of(&client, policy).await {
        if let Some(n) = child.metadata.name {
            let _ = backups.delete(&n, &DeleteParams::default()).await;
        }
    }
}

/// `groupBy: VolumeGroupSnapshot` captures every matched PVC in ONE shared
/// group, then stages each member from its own member snapshot.
#[tokio::test]
#[ignore = "requires the e2e harness + CSI snapshot stack (mise run //crates/e2e:snapshot-stack)"]
async fn a_group_capture_is_shared_by_every_member_and_reaped_after() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    // HARD requirement, not a skip: a harness missing the group CRDs must fail
    // loudly rather than masquerade as a pass (#121).
    let crds: Api<k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition> =
        Api::all(client.clone());
    assert!(
        crds.get_opt("volumegroupsnapshots.groupsnapshot.storage.k8s.io")
            .await
            .expect("list CRDs")
            .is_some(),
        "VolumeGroupSnapshot CRDs are absent — run `mise run //crates/e2e:snapshot-stack`"
    );

    ensure_repo(&client, "multipvc-group").await;
    for (name, marker) in [("e2e-grp-a", "alpha"), ("e2e-grp-b", "bravo")] {
        csi_pvc_with_data(&client, name, marker).await;
    }

    let repos: Api<kopiur_api::Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<kopiur_api::SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<kopiur_api::SnapshotSchedule> =
        Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<kopiur_api::Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    clear_scenario_leftovers(&client, "e2e-group-schedule", "e2e-group-policy").await;
    // A wedged try can also strand its VolumeGroupSnapshot (kopiur's reaper is
    // keyed on the members it just deleted above), and one stale group breaks
    // this test's "exactly ONE VolumeGroupSnapshot" invariant. Group names are
    // schedule-prefixed (`<schedule>-<ts>-<hash>-grp`), so the sweep is scoped.
    {
        let vgs = volume_group_snapshots(&client);
        for g in vgs.list(&ListParams::default()).await.expect("list").items {
            if let Some(n) = g.metadata.name
                && n.starts_with("e2e-group-schedule-")
            {
                let _ = vgs.delete(&n, &DeleteParams::default()).await;
            }
        }
        wait_until(
            "leftover VolumeGroupSnapshots are gone",
            default_timeout(),
            poll_interval(),
            || async {
                let none = !vgs
                    .list(&ListParams::default())
                    .await?
                    .items
                    .iter()
                    .any(|g| {
                        g.metadata
                            .name
                            .as_deref()
                            .is_some_and(|n| n.starts_with("e2e-group-schedule-"))
                    });
                Ok(none.then_some(()))
            },
        )
        .await
        .expect("a previous try's VolumeGroupSnapshot must clear before a fresh capture");
    }

    let repo = "e2e-repo-multipvc-group";
    let _ = repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                repo,
                "multipvc-group",
                serde_json::json!({}),
            )),
        )
        .await;
    wait_phase(&repos, repo, "Ready").await.expect("repo Ready");

    let policy = "e2e-group-policy";
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                policy,
                "Repository",
                repo,
                serde_json::json!({
                    "sources": [ {
                        "pvcSelector": {
                            "labelSelector": { "matchLabels": { BACKUP_LABEL_KEY: GROUP_LABEL_VALUE } }
                        }
                    } ],
                    // The DEFAULT. Example 04 does not set it either.
                    "groupBy": "VolumeGroupSnapshot",
                    "copyMethod": "Snapshot",
                    "identity": { "username": "grouped", "hostname": "e2e" }
                }),
            )),
        )
        .await
        .expect("create grouped SnapshotPolicy");
    assert_selector_landed(&policies, policy).await;

    let schedule = "e2e-group-schedule";
    create_idempotent(
        &schedules,
        &cr(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotSchedule",
            "metadata": { "name": schedule, "namespace": E2E_NAMESPACE },
            "spec": {
                "policyRef": { "name": policy },
                "schedule": { "cron": "0 3 * * *", "runOnCreate": true }
            }
        })),
        "create SnapshotSchedule",
    )
    .await;

    let vgs = volume_group_snapshots(&client);
    // EXACTLY ONE group for the whole expansion — that is the entire point. N
    // groups would mean N independent captures, i.e. no consistency at all.
    wait_until(
        "exactly one VolumeGroupSnapshot",
        default_timeout(),
        poll_interval(),
        || async {
            let n = vgs.list(&ListParams::default()).await?.items.len();
            Ok((n == 1).then_some(()))
        },
    )
    .await
    .expect("one shared group for the expansion");
    let group_name = vgs
        .list(&ListParams::default())
        .await
        .expect("list")
        .items
        .first()
        .and_then(|o| o.metadata.name.clone())
        .expect("the group exists");

    // Every member pins the SAME group and stages from its OWN member snapshot.
    wait_until(
        "both grouped Snapshots Succeeded",
        default_timeout(),
        poll_interval(),
        || async {
            let kids = children_of(&client, policy).await;
            let done = kids.iter().filter(|k| {
                k.status.as_ref().and_then(|s| s.phase.as_ref())
                    == Some(&kopiur_api::SnapshotPhase::Succeeded)
            });
            Ok((kids.len() == 2 && done.count() == 2).then_some(()))
        },
    )
    .await
    .expect("both members of a group capture should succeed");

    let mut member_snapshots = Vec::new();
    for child in children_of(&client, policy).await {
        let name = child.metadata.name.clone().expect("named");
        let pinned = child
            .spec
            .source
            .as_ref()
            .and_then(|s| s.group.as_ref())
            .unwrap_or_else(|| panic!("child {name} must pin the shared group"));
        assert_eq!(
            pinned.volume_group_snapshot_name, group_name,
            "every member must pin the SAME group object"
        );
        let s = status_json(&backups, &name).await;
        assert_eq!(
            s["staged"]["volumeGroupSnapshotName"].as_str(),
            Some(group_name.as_str()),
            "status must record which capture this backup came from"
        );
        member_snapshots.push(
            s["staged"]["volumeSnapshotName"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        );
    }
    member_snapshots.sort();
    member_snapshots.dedup();
    assert_eq!(
        member_snapshots.len(),
        2,
        "each member stages from its OWN member snapshot, not a shared one: \
         {member_snapshots:?}"
    );

    // The group has no ownerReferences, so nothing reclaims it but kopiur. Once
    // every member is terminal and its stage torn down, it must be gone.
    wait_until(
        "the shared group is reaped",
        default_timeout(),
        poll_interval(),
        || async {
            let gone = vgs.get_opt(&group_name).await?.is_none();
            Ok(gone.then_some(()))
        },
    )
    .await
    .expect("the shared VolumeGroupSnapshot must be reaped once every member is done");

    let _ = schedules.delete(schedule, &DeleteParams::default()).await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    for child in children_of(&client, policy).await {
        if let Some(n) = child.metadata.name {
            let _ = backups.delete(&n, &DeleteParams::default()).await;
        }
    }
}
