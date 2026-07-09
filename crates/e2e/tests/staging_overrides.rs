//! e2e: staged-PVC overrides (`spec.staging.storageClassName` / `accessModes`) and
//! the staging safeguards around them (the CephFS-shallow-clone feature, GitHub
//! issue: forgejo hourly hard-fail).
//!
//! 1. **Override happy path + Immediate bind gate**: a policy pointing staging at a
//!    runtime-created StorageClass (same provisioner as `csi-hostpath-sc`, but
//!    `volumeBindingMode: Immediate`) must produce a staged PVC carrying BOTH
//!    overrides, wait for it to bind (the new pre-Job bind gate — pre-fix code
//!    minted the mover Job against the unbound claim), back up successfully with a
//!    real `kopiaSnapshotID`, record the class in `status.staged`, and reap the
//!    stage. In kind this is the closest analogue to a real CephFS
//!    `backingSnapshot: "true"` setup, which needs rook-ceph and is not locally
//!    testable — the unit tests on `build_staged_pvc`/`pvc_bind_outcome` plus this
//!    scenario cover the mechanism.
//! 2. **Same-driver preflight fast fail**: an override class on a different
//!    (nonexistent) provisioner must fail the Snapshot terminally with
//!    `StagedClassMismatch` BEFORE creating any VolumeSnapshot, staged PVC, or
//!    mover Job — pre-fix shape of this misconfig was a mover pod judged
//!    `MoverPodWedged` at 300 s whose cleanup deleted the VolumeSnapshot while the
//!    CSI restore still consumed it.
//!
//! Requires the CSI snapshot stack (`mise run //crates/e2e:snapshot-stack`) like
//! `copy_methods.rs`. Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; skips
//! gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use kube::Api;
use kube::api::{DeleteParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind, ObjectMeta};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use k8s_openapi::api::storage::v1::StorageClass;
use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

/// The storage class the `snapshot-stack` mise task installs (WaitForFirstConsumer).
const CSI_STORAGE_CLASS: &str = "csi-hostpath-sc";
/// Runtime-created override class: hostpath provisioner, but Immediate binding —
/// exercises the pre-Job bind gate's Bound→Ready path.
const IMMEDIATE_SC: &str = "e2e-ovr-immediate";
/// Runtime-created wrong-driver class for the preflight fast-fail scenario.
const ALIEN_SC: &str = "e2e-ovr-alien";
const ALIEN_PROVISIONER: &str = "example.com/no-such-driver";

fn volume_snapshots(client: &kube::Client) -> Api<DynamicObject> {
    let ar = ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("snapshot.storage.k8s.io", "v1", "VolumeSnapshot"),
        "volumesnapshots",
    );
    Api::namespaced_with(client.clone(), E2E_NAMESPACE, &ar)
}

/// The snapshot stack must be installed (hard requirement, mirrors
/// `copy_methods.rs`); returns the hostpath driver's provisioner for building the
/// Immediate override class.
async fn require_snapshot_stack(client: &kube::Client) -> String {
    let scs: Api<StorageClass> = Api::all(client.clone());
    let base = scs
        .get_opt(CSI_STORAGE_CLASS)
        .await
        .expect("list storageclasses");
    let Some(base) = base else {
        panic!(
            "storageclass {CSI_STORAGE_CLASS} not found — run `mise run \
             //crates/e2e:snapshot-stack` (or set KOPIUR_E2E_SKIP_SNAPSHOT_STACK=1 only \
             for shards excluding this file)"
        );
    };
    base.provisioner
}

/// Create a StorageClass idempotently (409 = a previous run left it; fine — the
/// spec is deterministic).
async fn ensure_storage_class(
    client: &kube::Client,
    name: &str,
    provisioner: &str,
    binding_mode: &str,
) {
    let scs: Api<StorageClass> = Api::all(client.clone());
    let sc = StorageClass {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            ..Default::default()
        },
        provisioner: provisioner.to_string(),
        reclaim_policy: Some("Delete".to_string()),
        volume_binding_mode: Some(binding_mode.to_string()),
        ..Default::default()
    };
    match scs.create(&PostParams::default(), &sc).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 409 => {}
        Err(e) => panic!("create StorageClass {name}: {e}"),
    }
}

/// OVERRIDE HAPPY PATH: the staged PVC carries `spec.staging.storageClassName` +
/// `accessModes`, the Immediate bind gate waits for it to bind, the backup reads
/// the stage and Succeeds with a real kopia snapshot, `status.staged` records the
/// effective class, and the stage is reaped.
#[tokio::test]
#[ignore = "requires the e2e harness + the CSI snapshot stack (mise run //crates/e2e:test)"]
async fn staging_overrides_stage_on_an_immediate_class_and_succeed() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    let provisioner = require_snapshot_stack(&client).await;
    ensure_storage_class(&client, IMMEDIATE_SC, &provisioner, "Immediate").await;
    ensure_repo(&client, "staging-override").await;

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // A CSI-provisioned source PVC with real content (the VS restore must have
    // something to provision from), bound by a one-shot seed pod.
    let src = "e2e-ovr-src-pvc";
    pvcs.create(
        &PostParams::default(),
        &cr(serde_json::json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": { "name": src, "namespace": E2E_NAMESPACE },
            "spec": {
                "accessModes": ["ReadWriteOnce"],
                "storageClassName": CSI_STORAGE_CLASS,
                "resources": { "requests": { "storage": "64Mi" } },
            },
        })),
    )
    .await
    .expect("create CSI source PVC");
    pods.create(
        &PostParams::default(),
        &cr(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": { "name": "e2e-ovr-seed", "namespace": E2E_NAMESPACE },
            "spec": {
                "restartPolicy": "Never",
                "containers": [{
                    "name": "seed", "image": kopiur_e2e::consts::BUSYBOX_IMAGE,
                    "imagePullPolicy": "IfNotPresent",
                    "command": ["sh", "-c", "echo kopiur-override > /data/marker.txt"],
                    "volumeMounts": [{ "name": "d", "mountPath": "/data" }],
                }],
                "volumes": [{ "name": "d", "persistentVolumeClaim": { "claimName": src } }],
            },
        })),
    )
    .await
    .expect("create seed pod");
    wait_until(
        "CSI source PVC Bound",
        default_timeout(),
        poll_interval(),
        || async {
            let bound = pvcs
                .get_opt(src)
                .await?
                .and_then(|p| p.status.and_then(|s| s.phase))
                .as_deref()
                == Some("Bound");
            Ok(bound.then_some(()))
        },
    )
    .await
    .expect("source PVC should bind");

    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                "e2e-ovr-repo",
                "staging-override",
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Repository");
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                "e2e-ovr-policy",
                "Repository",
                "e2e-ovr-repo",
                serde_json::json!({
                    "copyMethod": "Snapshot",
                    "sources": [ { "pvc": { "name": src } } ],
                    "staging": {
                        "storageClassName": IMMEDIATE_SC,
                        "accessModes": ["ReadWriteOnce"],
                    },
                }),
            )),
        )
        .await
        .expect("create SnapshotPolicy with staging overrides");
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-ovr-backup",
                "e2e-ovr-policy",
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Snapshot");

    // The staged PVC exists mid-run and carries BOTH overrides — captured before
    // the post-success reap. Pre-override code copied the source's class verbatim.
    let staged_pvc_name = "e2e-ovr-backup-src";
    let staged_spec = wait_until(
        "staged PVC exists with the override shape",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(pvcs
                .get_opt(staged_pvc_name)
                .await?
                .and_then(|p| p.spec)
                .map(Box::new))
        },
    )
    .await
    .expect("staged PVC should be created");
    assert_eq!(
        staged_spec.storage_class_name.as_deref(),
        Some(IMMEDIATE_SC),
        "the staged PVC must use spec.staging.storageClassName, not the source's class"
    );
    assert_eq!(
        staged_spec.access_modes.as_deref(),
        Some(["ReadWriteOnce".to_string()].as_slice()),
        "the staged PVC must use spec.staging.accessModes"
    );

    // The backup succeeds reading the override-staged PVC (the bind gate held the
    // Job until the Immediate-class claim was Bound).
    wait_phase(&backups, "e2e-ovr-backup", "Succeeded")
        .await
        .expect("Snapshot over the override-staged source should Succeed");
    let status = status_json(&backups, "e2e-ovr-backup").await;
    assert!(
        status
            .pointer("/snapshot/kopiaSnapshotID")
            .and_then(|v| v.as_str())
            .is_some_and(|id| !id.is_empty()),
        "a real kopia snapshot must have been produced: {status}"
    );
    let staged = status.get("staged").cloned().unwrap_or_default();
    assert_eq!(
        staged.get("storageClassName").and_then(|v| v.as_str()),
        Some(IMMEDIATE_SC),
        "status.staged must record the effective (override) class: {status}"
    );
    assert!(
        staged
            .get("stagingTimeoutSeconds")
            .and_then(|v| v.as_i64())
            .is_some_and(|s| s > 0),
        "status.staged must pin the resolved staging budget: {status}"
    );

    // The stage is reaped after success.
    wait_until(
        "staged PVC reaped",
        default_timeout(),
        poll_interval(),
        || async { Ok(pvcs.get_opt(staged_pvc_name).await?.is_none().then_some(())) },
    )
    .await
    .expect("staged PVC should be cleaned up after the backup succeeds");

    // Cleanup (tests run --test-threads=1; the SC is ours to remove).
    let _ = backups
        .delete("e2e-ovr-backup", &DeleteParams::default())
        .await;
    let _ = policies
        .delete("e2e-ovr-policy", &DeleteParams::default())
        .await;
    let _ = repos.delete("e2e-ovr-repo", &DeleteParams::default()).await;
    let _ = pods.delete("e2e-ovr-seed", &DeleteParams::default()).await;
    let _ = pvcs.delete(src, &DeleteParams::default()).await;
    let scs: Api<StorageClass> = Api::all(client.clone());
    let _ = scs.delete(IMMEDIATE_SC, &DeleteParams::default()).await;
}

/// PREFLIGHT FAST FAIL: an override class on a foreign driver fails the Snapshot
/// terminally with `StagedClassMismatch` — naming both provisioners — before ANY
/// staging object or mover Job exists. This is the regression trip-wire for the
/// original bug shape: pre-fix, a never-binding staged PVC surfaced as
/// `MoverPodWedged` after 300 s and the cleanup deleted the VolumeSnapshot while
/// the CSI restore still consumed it.
#[tokio::test]
#[ignore = "requires the e2e harness + the CSI snapshot stack (mise run //crates/e2e:test)"]
async fn staging_override_on_a_foreign_driver_fails_fast_with_class_mismatch() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    let source_provisioner = require_snapshot_stack(&client).await;
    ensure_storage_class(&client, ALIEN_SC, ALIEN_PROVISIONER, "Immediate").await;
    ensure_repo(&client, "staging-mismatch").await;

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // The source PVC only needs to EXIST with a CSI class (the preflight reads its
    // provisioner; binding is irrelevant — no seed pod).
    let src = "e2e-mis-src-pvc";
    pvcs.create(
        &PostParams::default(),
        &cr(serde_json::json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": { "name": src, "namespace": E2E_NAMESPACE },
            "spec": {
                "accessModes": ["ReadWriteOnce"],
                "storageClassName": CSI_STORAGE_CLASS,
                "resources": { "requests": { "storage": "64Mi" } },
            },
        })),
    )
    .await
    .expect("create CSI source PVC");

    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                "e2e-mis-repo",
                "staging-mismatch",
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Repository");
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                "e2e-mis-policy",
                "Repository",
                "e2e-mis-repo",
                serde_json::json!({
                    "copyMethod": "Snapshot",
                    "sources": [ { "pvc": { "name": src } } ],
                    "staging": { "storageClassName": ALIEN_SC },
                }),
            )),
        )
        .await
        .expect("create SnapshotPolicy with a wrong-driver override");
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-mis-backup",
                "e2e-mis-policy",
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Snapshot");

    // Terminal fast fail with the specific reason; the message names BOTH drivers
    // so the user knows exactly which class to fix.
    let cond = wait_condition(&backups, "e2e-mis-backup", "SourceStaged", "False")
        .await
        .expect("SourceStaged=False condition");
    assert_eq!(
        cond.get("reason").and_then(|r| r.as_str()),
        Some("StagedClassMismatch"),
        "expected the same-driver preflight reason; condition was {cond:?}"
    );
    let msg = cond
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    assert!(
        msg.contains(ALIEN_PROVISIONER) && msg.contains(&source_provisioner),
        "the message must name both provisioners: {msg}"
    );
    wait_phase(&backups, "e2e-mis-backup", "Failed")
        .await
        .expect("Snapshot reaches Failed");
    let status = status_json(&backups, "e2e-mis-backup").await;
    let stalled = status
        .get("conditions")
        .and_then(|c| c.as_array())
        .and_then(|a| {
            a.iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("Stalled"))
        })
        .and_then(|c| c.get("status"))
        .and_then(|s| s.as_str());
    assert_eq!(
        stalled,
        Some("True"),
        "terminal failure must be Stalled=True"
    );

    // Nothing was ever created: no mover Job, no VolumeSnapshot, no staged PVC.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    assert!(
        jobs.get_opt("e2e-mis-backup")
            .await
            .expect("get job")
            .is_none(),
        "no mover Job may exist for a preflight-failed backup"
    );
    assert!(
        volume_snapshots(&client)
            .get_opt("e2e-mis-backup-snap")
            .await
            .expect("get VS")
            .is_none(),
        "the preflight must fail BEFORE creating a VolumeSnapshot"
    );
    assert!(
        pvcs.get_opt("e2e-mis-backup-src")
            .await
            .expect("get staged pvc")
            .is_none(),
        "the preflight must fail BEFORE creating a staged PVC"
    );

    // Cleanup.
    let _ = backups
        .delete("e2e-mis-backup", &DeleteParams::default())
        .await;
    let _ = policies
        .delete("e2e-mis-policy", &DeleteParams::default())
        .await;
    let _ = repos.delete("e2e-mis-repo", &DeleteParams::default()).await;
    let _ = pvcs.delete(src, &DeleteParams::default()).await;
    let scs: Api<StorageClass> = Api::all(client.clone());
    let _ = scs.delete(ALIEN_SC, &DeleteParams::default()).await;
}
