//! e2e: repository/restore lifecycle behaviors — Restore `target.populator: {}`
//! (ADR-0005 §9), `mode: ReadOnly` (§11), and kstatus `Ready` for `kubectl wait`
//! (§2).
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use kube::Api;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use k8s_openapi::api::events::v1::Event;
use k8s_openapi::api::storage::v1::StorageClass;
use kopiur_api::{Repository, Restore, Snapshot, SnapshotPolicy};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, builders, default_timeout, poll_interval,
    scrape_controller_metrics, wait, wait_until,
};

/// CSI hostpath StorageClass installed by the `snapshot-stack` harness step — a
/// populator-aware provisioner (its external-provisioner defers to `dataSourceRef`).
/// `Immediate` binding (provisions the prime PVC as soon as it's created).
const CSI_STORAGE_CLASS: &str = "csi-hostpath-sc";
/// The `WaitForFirstConsumer` variant over the same hostpath provisioner (also installed
/// by `snapshot-stack`). Exercises the populator handshake's late-binding path: the claim
/// only gets a `selected-node` once a pod schedules it, which the controller pins the
/// prime PVC to. See [`restore_populator_wffc_binds_pvc_and_restores_data`].
const CSI_STORAGE_CLASS_WFFC: &str = "csi-hostpath-sc-wffc";

/// `Restore.spec.target.populator: {}` (ADR-0005 §9): the explicit passive-populator
/// target form is accepted and threads through to a restore mover Job. (The empty
/// `target` form was removed; this proves the replacement form is wired.)
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn restore_populator_target_form_is_accepted() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_seed(
        &client,
        "e2e-pop-repo",
        "e2e-pop-policy",
        "e2e-pop-seed",
        "populator",
    )
    .await;

    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let name = "e2e-pop-restore";
    restores
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Restore",
                "metadata": { "name": name, "namespace": E2E_NAMESPACE },
                "spec": {
                    "repository": { "kind": "Repository", "name": "e2e-pop-repo" },
                    "source": { "snapshotRef": { "name": "e2e-pop-seed" } },
                    "target": { "populator": {} }
                }
            })),
        )
        .await
        .expect("create Restore with target.populator:{} (the explicit populator form)");

    // Populator mode is PASSIVE (ADR-0005 §9): the Restore is admitted and parks in
    // `AwaitingClaim` until a PVC references it via `dataSourceRef` — it does NOT eagerly
    // build a mover Job. Asserting it reaches `AwaitingClaim=True` proves the explicit
    // `target.populator: {}` form is accepted and wired through to the populator machine.
    wait_condition(&restores, name, "AwaitingClaim", "True")
        .await
        .expect(
            "a populator Restore must reach AwaitingClaim=True (passive, awaiting a PVC claim)",
        );
    let _ = restores.delete(name, &DeleteParams::default()).await;
}

/// Whether `storage_class` is present (proceed with the test). If it's absent we either
/// HARD-FAIL or skip: a `csi: true` CI shard installs the snapshot stack and sets
/// `KOPIUR_E2E_REQUIRE_CSI=1`, so there an absent class is a real setup failure and must
/// NOT silently pass — a silent skip once let a populator regression ship green (#121).
/// Without that env (local dev with no snapshot stack) we skip gracefully.
async fn csi_class_present_or_skip(client: &kube::Client, storage_class: &str) -> bool {
    let scs: Api<StorageClass> = Api::all(client.clone());
    if scs
        .get_opt(storage_class)
        .await
        .expect("list storageclasses")
        .is_some()
    {
        return true;
    }
    let require = std::env::var("KOPIUR_E2E_REQUIRE_CSI").is_ok_and(|v| v == "1");
    assert!(
        !require,
        "storageclass {storage_class} absent but KOPIUR_E2E_REQUIRE_CSI=1 — this shard must \
         install the CSI snapshot stack (mise run //crates/e2e:snapshot-stack) before the \
         populator/copyMethod tests; refusing to silently skip (cf. #121)"
    );
    eprintln!(
        "skipping populator test: storageclass {storage_class} absent \
         (run `mise run //crates/e2e:snapshot-stack`)"
    );
    false
}

/// Shared body for the populator data-integrity tests (ADR-0005 §9): given an
/// already-seeded `repo`/`seed`, create a populator `Restore`, a claiming PVC whose
/// `dataSourceRef` points at it on `storage_class`, and a reader pod that asserts the
/// seed source's `a.txt`. The pod schedules the claim (also producing the `selected-node`
/// a `WaitForFirstConsumer` class needs) and can only run once the claim BINDS, so its
/// success proves BOTH the prime→consumer rebind and the restored bytes. Then assert the
/// `Restore` settles `Completed` — the regression guard for the #121 wedge, where the
/// mover stamped `Completed` before the rebind, leaving the claim `Pending` forever.
/// `prefix` namespaces the per-test object names so the binding-mode variants don't clash.
async fn assert_populator_binds_and_restores(
    client: &kube::Client,
    storage_class: &str,
    repo: &str,
    seed: &str,
    prefix: &str,
) {
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let restore_name = format!("{prefix}-restore");
    restores
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Restore",
                "metadata": { "name": restore_name, "namespace": E2E_NAMESPACE },
                "spec": {
                    "repository": { "kind": "Repository", "name": repo },
                    "source": { "snapshotRef": { "name": seed } },
                    "target": { "populator": {} }
                }
            })),
        )
        .await
        .expect("create populator Restore");

    // The claiming PVC: its dataSourceRef points at the Restore, so a populator-aware
    // provisioner defers to the handshake instead of binding it to an empty volume.
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let claim = format!("{prefix}-data");
    pvcs.create(
        &PostParams::default(),
        &cr(serde_json::json!({
            "apiVersion": "v1",
            "kind": "PersistentVolumeClaim",
            "metadata": { "name": claim, "namespace": E2E_NAMESPACE },
            "spec": {
                "accessModes": ["ReadWriteOnce"],
                "storageClassName": storage_class,
                "resources": { "requests": { "storage": "1Gi" } },
                "dataSourceRef": {
                    "apiGroup": "kopiur.home-operations.com",
                    "kind": "Restore",
                    "name": restore_name,
                }
            }
        })),
    )
    .await
    .expect("create claiming PVC");

    // One pod does double duty: scheduling it produces the `selected-node` a
    // WaitForFirstConsumer claim needs (the controller pins the prime PVC to it), and it
    // asserts the restored bytes — `a.txt` is "hello kopiur e2e" in the seed source. It
    // can only run once the claim binds, so its success proves both the bind and the data.
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let reader = format!("{prefix}-reader");
    pods.create(
        &PostParams::default(),
        &builders::one_shot_pod(
            E2E_NAMESPACE,
            &reader,
            &[
                "sh",
                "-c",
                "test \"$(cat /mnt/a.txt)\" = 'hello kopiur e2e'",
            ],
            &[(claim.as_str(), "/mnt")],
        ),
    )
    .await
    .expect("create reader pod");

    wait::pod_succeeded(client, E2E_NAMESPACE, &reader)
        .await
        .expect("the claiming PVC binds and a.txt is restored into it");
    wait_phase(&restores, &restore_name, "Completed")
        .await
        .expect("the populator Restore reaches Completed once the claim is bound");

    let _ = pods.delete(&reader, &DeleteParams::default()).await;
    let _ = pvcs.delete(&claim, &DeleteParams::default()).await;
    let _ = restores
        .delete(&restore_name, &DeleteParams::default())
        .await;
}

/// ADR-0005 §9 end-to-end (Immediate binding): a PVC whose `dataSourceRef` claims a
/// populator `Restore` is filled via the prime-PVC/rebind handshake and binds carrying
/// the snapshot's data (the seed source's `a.txt`).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install + CSI snapshot stack"]
async fn restore_populator_binds_pvc_and_restores_data() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    // Needs a populator-aware CSI provisioner whose external-provisioner defers to a
    // populator `dataSourceRef` (the default local-path would bind the claim to an empty
    // volume). The harness installs `csi-hostpath-sc` via the snapshot-stack step.
    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }

    ensure_seed(
        &client,
        "e2e-pop2-repo",
        "e2e-pop2-policy",
        "e2e-pop2-seed",
        "populator2",
    )
    .await;

    assert_populator_binds_and_restores(
        &client,
        CSI_STORAGE_CLASS,
        "e2e-pop2-repo",
        "e2e-pop2-seed",
        "e2e-pop2",
    )
    .await;
}

/// ADR-0005 §9 end-to-end (WaitForFirstConsumer binding): the late-binding path of the
/// populator handshake. The claim only gets a `selected-node` once the reader pod
/// schedules it; the controller then pins the prime PVC to that node (the
/// `consumer_storage_class_is_wffc` / `AwaitingPodSchedule` path in restore.rs) before
/// restoring + rebinding. Reuses the `populator2` seed (restores are read-only and the
/// tests run sequentially), with its own object names + the WFFC StorageClass.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install + CSI snapshot stack"]
async fn restore_populator_wffc_binds_pvc_and_restores_data() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS_WFFC).await {
        return;
    }

    ensure_seed(
        &client,
        "e2e-pop2-repo",
        "e2e-pop2-policy",
        "e2e-pop2-seed",
        "populator2",
    )
    .await;

    assert_populator_binds_and_restores(
        &client,
        CSI_STORAGE_CLASS_WFFC,
        "e2e-pop2-repo",
        "e2e-pop2-seed",
        "e2e-pop3",
    )
    .await;
}

/// The `Resolved` condition's reason on a Restore status, if present.
fn resolved_reason(status: &serde_json::Value) -> Option<String> {
    status["conditions"]
        .as_array()?
        .iter()
        .find(|c| c["type"] == "Resolved")
        .and_then(|c| c["reason"].as_str())
        .map(str::to_string)
}

/// Shared body for the deploy-or-restore (`onMissingSnapshot: Continue`) populator
/// regression: given an already-seeded EMPTY `policy` (a Ready repo with NO snapshot),
/// create a populator `Restore` whose `fromPolicy` source resolves to nothing, a claiming
/// PVC on `storage_class`, and a pod that writes+reads a file in the mount. With no
/// snapshot the controller must STILL provision an empty volume (an empty prime PVC +
/// rebind) so the claim BINDS and the pod runs — pre-fix the claim hung `Pending` forever
/// ("Assuming an external populator will provision the volume"). Also asserts the
/// no-snapshot decision is pinned to `status.resolved.resolution: NoSnapshot`, so a
/// snapshot that appears later can never silently restore over the in-use volume.
async fn assert_populator_continue_provisions_empty(
    client: &kube::Client,
    storage_class: &str,
    policy: &str,
    prefix: &str,
) {
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let restore_name = format!("{prefix}-restore");
    restores
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Restore",
                "metadata": { "name": restore_name, "namespace": E2E_NAMESPACE },
                "spec": {
                    "source": { "fromPolicy": { "name": policy } },
                    // explicit for clarity (it's also the fromPolicy default)
                    "policy": { "onMissingSnapshot": "Continue" },
                    "target": { "populator": {} }
                }
            })),
        )
        .await
        .expect("create deploy-or-restore populator Restore");

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let claim = format!("{prefix}-data");
    pvcs.create(
        &PostParams::default(),
        &cr(serde_json::json!({
            "apiVersion": "v1",
            "kind": "PersistentVolumeClaim",
            "metadata": { "name": claim, "namespace": E2E_NAMESPACE },
            "spec": {
                "accessModes": ["ReadWriteOnce"],
                "storageClassName": storage_class,
                "resources": { "requests": { "storage": "1Gi" } },
                "dataSourceRef": {
                    "apiGroup": "kopiur.home-operations.com",
                    "kind": "Restore",
                    "name": restore_name,
                }
            }
        })),
    )
    .await
    .expect("create claiming PVC");

    // The pod writes then reads a file in the empty mount: it schedules the claim (so a
    // WaitForFirstConsumer class gets its `selected-node`) and can only run once the claim
    // BINDS, so its success proves the empty volume was provisioned, rebound, and is usable.
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let probe = format!("{prefix}-probe");
    pods.create(
        &PostParams::default(),
        &builders::one_shot_pod(
            E2E_NAMESPACE,
            &probe,
            &[
                "sh",
                "-c",
                "echo ok > /mnt/probe && test \"$(cat /mnt/probe)\" = ok",
            ],
            &[(claim.as_str(), "/mnt")],
        ),
    )
    .await
    .expect("create probe pod");

    wait::pod_succeeded(client, E2E_NAMESPACE, &probe)
        .await
        .expect("the claiming PVC binds to a fresh empty volume and is writable");
    wait_phase(&restores, &restore_name, "Completed")
        .await
        .expect("the deploy-or-restore populator Restore reaches Completed");

    let s = status_json(&restores, &restore_name).await;
    assert_eq!(
        s["resolved"]["resolution"],
        serde_json::json!("NoSnapshot"),
        "the no-snapshot decision must be pinned so a later snapshot can't retarget it: {s}"
    );
    assert_eq!(
        resolved_reason(&s).as_deref(),
        Some("NoSnapshotContinue"),
        "status: {s}"
    );

    let _ = pods.delete(&probe, &DeleteParams::default()).await;
    let _ = pvcs.delete(&claim, &DeleteParams::default()).await;
    let _ = restores
        .delete(&restore_name, &DeleteParams::default())
        .await;
}

/// Deploy-or-restore, the user-reported regression (WaitForFirstConsumer, mirroring the
/// reporter's `openebs-zfspv`): a populator `Restore` over a policy with NO snapshot +
/// `onMissingSnapshot: Continue` must provision a FRESH EMPTY volume so the claiming PVC
/// binds and the workload pod starts. Pre-fix the controller stamped the Restore
/// `Completed` but returned before the populator handshake, leaving the PVC `Pending`
/// forever. The pod scheduling the claim drives the WFFC `selected-node` path (the empty
/// prime PVC is pinned to that node, provisioned pod-less, then rebound).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install + CSI snapshot stack"]
async fn restore_populator_continue_provisions_empty_volume_wffc() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS_WFFC).await {
        return;
    }

    ensure_empty_policy(
        &client,
        "e2e-popempty-repo",
        "e2e-popempty-policy",
        "popempty",
    )
    .await;
    assert_populator_continue_provisions_empty(
        &client,
        CSI_STORAGE_CLASS_WFFC,
        "e2e-popempty-policy",
        "e2e-popempty-wffc",
    )
    .await;
}

/// Deploy-or-restore, Immediate-binding variant: the empty prime PVC provisions as soon as
/// it's created (no pod-schedule dance), so the claim binds without the WFFC `selected-node`
/// step. Same invariant: a populator `Continue` with no snapshot comes up as an empty,
/// usable volume.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install + CSI snapshot stack"]
async fn restore_populator_continue_provisions_empty_volume_immediate() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }

    ensure_empty_policy(
        &client,
        "e2e-popempty-repo",
        "e2e-popempty-policy",
        "popempty",
    )
    .await;
    assert_populator_continue_provisions_empty(
        &client,
        CSI_STORAGE_CLASS,
        "e2e-popempty-policy",
        "e2e-popempty-imm",
    )
    .await;
}

/// Deploy-or-restore for a DIRECT `target.pvc`: the same early-return also stranded an
/// operator-created target PVC (it was never created when there was no snapshot). With the
/// fix the controller creates the empty PVC and completes. Asserts the target PVC now
/// exists and the Restore completes with the empty-volume reason (NOT "snapshot data was
/// written"). No CSI provisioner needed — the default storage class provisions the PVC.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn restore_direct_pvc_continue_provisions_empty_volume() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    ensure_empty_policy(
        &client,
        "e2e-popempty-repo",
        "e2e-popempty-policy",
        "popempty",
    )
    .await;

    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let name = "e2e-direct-empty";
    let target_pvc = "e2e-direct-empty-dst";
    let _ = restores.delete(name, &DeleteParams::default()).await;
    restores
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Restore",
                "metadata": { "name": name, "namespace": E2E_NAMESPACE },
                "spec": {
                    "source": { "fromPolicy": { "name": "e2e-popempty-policy" } },
                    "policy": { "onMissingSnapshot": "Continue" },
                    "target": { "pvc": { "name": target_pvc, "capacity": "1Gi" } }
                }
            })),
        )
        .await
        .expect("create direct target.pvc deploy-or-restore Restore");

    wait_phase(&restores, name, "Completed")
        .await
        .expect("direct deploy-or-restore must complete by provisioning an empty PVC");

    // The operator-created target PVC must now exist (pre-fix it was never created).
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let created = pvcs
        .get_opt(target_pvc)
        .await
        .expect("get target PVC")
        .is_some();
    assert!(
        created,
        "deploy-or-restore must provision the operator-created target PVC {target_pvc}"
    );

    let s = status_json(&restores, name).await;
    assert_eq!(
        resolved_reason(&s).as_deref(),
        Some("NoSnapshotContinue"),
        "the empty completion must keep the deploy-or-restore reason (not 'RestoreSucceeded'): {s}"
    );

    let _ = restores.delete(name, &DeleteParams::default()).await;
    let _ = pvcs.delete(target_pvc, &DeleteParams::default()).await;
}

/// Data-safety: once deploy-or-restore comes up empty, the pinned `resolution: NoSnapshot`
/// decision must hold even if a snapshot for the policy appears LATER — the controller must
/// NOT re-resolve and restore over the (now bound, in-use) volume. Provisions the empty
/// volume, then seeds a snapshot for the same policy, forces a Restore reconcile, and
/// asserts the Restore stays `Completed`/`NoSnapshot` and the consumer keeps its same PV.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install + CSI snapshot stack"]
async fn restore_populator_continue_pins_decision_against_late_snapshot() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }

    // Own ISOLATED repo (popempty2): this test seeds a snapshot partway through, which
    // shares the policy source's kopia identity — reusing the shared `popempty` repo would
    // make it non-empty and break the sibling empty-volume tests depending on run order.
    let policy = "e2e-popempty2-policy";
    ensure_empty_policy(&client, "e2e-popempty2-repo", policy, "popempty2").await;

    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let prefix = "e2e-popempty-pin";
    let restore_name = format!("{prefix}-restore");
    let claim = format!("{prefix}-data");
    restores
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Restore",
                "metadata": { "name": restore_name, "namespace": E2E_NAMESPACE },
                "spec": {
                    "source": { "fromPolicy": { "name": policy } },
                    "policy": { "onMissingSnapshot": "Continue" },
                    "target": { "populator": {} }
                }
            })),
        )
        .await
        .expect("create populator Restore");

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    pvcs.create(
        &PostParams::default(),
        &cr(serde_json::json!({
            "apiVersion": "v1",
            "kind": "PersistentVolumeClaim",
            "metadata": { "name": claim, "namespace": E2E_NAMESPACE },
            "spec": {
                "accessModes": ["ReadWriteOnce"],
                "storageClassName": CSI_STORAGE_CLASS,
                "resources": { "requests": { "storage": "1Gi" } },
                "dataSourceRef": {
                    "apiGroup": "kopiur.home-operations.com",
                    "kind": "Restore",
                    "name": restore_name,
                }
            }
        })),
    )
    .await
    .expect("create claiming PVC");

    wait_phase(&restores, &restore_name, "Completed")
        .await
        .expect("empty volume provisioned + bound");
    let bound_pv = wait_until(
        &format!("{claim} bound to a PV"),
        default_timeout(),
        poll_interval(),
        || async {
            Ok(pvcs
                .get_opt(&claim)
                .await?
                .and_then(|p| p.spec?.volume_name))
        },
    )
    .await
    .expect("the claim binds to a PV");

    // Now a snapshot for the SAME policy appears (the app took its first backup).
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let late = format!("{prefix}-late-snap");
    let _ = backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                &late,
                policy,
                serde_json::json!({}),
            )),
        )
        .await;
    wait_phase(&backups, &late, "Succeeded")
        .await
        .expect("a later snapshot for the policy now exists");

    // Force the Restore to reconcile (its post-Completed requeue is long) by touching an
    // annotation; the pinned NoSnapshot decision must make it a no-op — no re-resolve.
    restores
        .patch(
            &restore_name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "metadata": { "annotations": { "e2e.kopiur/poke": "1" } }
            })),
        )
        .await
        .expect("poke the Restore to force a reconcile");

    // Give the controller several reconcile cycles, then assert nothing retargeted.
    for _ in 0..10 {
        tokio::time::sleep(poll_interval()).await;
        let s = status_json(&restores, &restore_name).await;
        assert_eq!(s["phase"], serde_json::json!("Completed"), "status: {s}");
        assert_eq!(
            s["resolved"]["resolution"],
            serde_json::json!("NoSnapshot"),
            "the pinned decision must not flip to a snapshot: {s}"
        );
    }
    let still = pvcs
        .get(&claim)
        .await
        .expect("get claim")
        .spec
        .and_then(|s| s.volume_name);
    assert_eq!(
        still.as_deref(),
        Some(bound_pv.as_str()),
        "the consumer must keep its original empty PV — no destructive re-restore"
    );

    let _ = backups.delete(&late, &DeleteParams::default()).await;
    let _ = pvcs.delete(&claim, &DeleteParams::default()).await;
    let _ = restores
        .delete(&restore_name, &DeleteParams::default())
        .await;
}

/// `mode: ReadOnly` (ADR-0005 §11): a ReadOnly repository serves restores but the
/// controller REFUSES backups against it. A Snapshot whose policy points at a ReadOnly
/// repo must not produce a snapshot; a Restore against it works.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn readonly_repo_refuses_backup_but_allows_restore() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    // First seed a snapshot via a READWRITE repo over the subdir, so there is data to
    // restore once we flip a repo to ReadOnly over the same subdir.
    ensure_seed(
        &client,
        "e2e-ro-rw-repo",
        "e2e-ro-rw-policy",
        "e2e-ro-seed",
        "readonly",
    )
    .await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // A ReadOnly repo over the same subdir (create disabled — it already exists).
    let ro_repo = "e2e-ro-repo";
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                ro_repo,
                "readonly",
                serde_json::json!({ "mode": "ReadOnly", "create": { "enabled": false } }),
            )),
        )
        .await
        .expect("create ReadOnly Repository");
    wait_phase(&repos, ro_repo, "Ready")
        .await
        .expect("ReadOnly repo should connect to Ready");

    // A backup against the ReadOnly repo must be refused: it never reaches Succeeded
    // and surfaces a not-Ready/blocked condition rather than writing to the repo.
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                "e2e-ro-policy",
                "Repository",
                ro_repo,
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create SnapshotPolicy against ReadOnly repo");
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-ro-backup",
                "e2e-ro-policy",
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Snapshot against ReadOnly repo");

    // The backup must be refused: phase Failed + RepositoryWritable=False
    // (reason RepositoryReadOnly), and it must never reach Succeeded.
    let cond = wait_condition(&backups, "e2e-ro-backup", "RepositoryWritable", "False")
        .await
        .expect("a Snapshot against a ReadOnly repository must surface RepositoryWritable=False");
    assert_eq!(
        cond.get("reason").and_then(|r| r.as_str()),
        Some("RepositoryReadOnly"),
        "the refusal reason must be RepositoryReadOnly"
    );
    assert_eq!(
        status_json(&backups, "e2e-ro-backup")
            .await
            .get("phase")
            .and_then(|p| p.as_str()),
        Some("Failed"),
        "a refused backup against a ReadOnly repository must be phase Failed"
    );

    // The refusal must also be counted: `kopiur_snapshot_refusals_total` with
    // reason=RepositoryReadOnly is the only aggregate signal (the reconcile
    // returns Ok, so reconcile_errors never sees it). Scraped through the
    // Service proxy like the observability scenarios.
    wait_until(
        "kopiur_snapshot_refusals_total{reason=RepositoryReadOnly} >= 1",
        default_timeout(),
        poll_interval(),
        || async {
            let text = scrape_controller_metrics(&client).await.unwrap_or_default();
            let found = text.lines().any(|l| {
                l.starts_with("kopiur_snapshot_refusals_total")
                    && l.contains("reason=\"RepositoryReadOnly\"")
                    && l.contains("name=\"e2e-ro-backup\"")
                    && l.split_whitespace()
                        .last()
                        .and_then(|v| v.parse::<f64>().ok())
                        .is_some_and(|v| v >= 1.0)
            });
            Ok(found.then_some(()))
        },
    )
    .await
    .expect("the ReadOnly refusal must increment kopiur_snapshot_refusals_total");

    // A Restore against the ReadOnly repo WORKS (serves reads): Completed.
    restores
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Restore",
                "metadata": { "name": "e2e-ro-restore", "namespace": E2E_NAMESPACE },
                "spec": {
                    "repository": { "kind": "Repository", "name": ro_repo },
                    "source": { "snapshotRef": { "name": "e2e-ro-seed" } },
                    "target": { "pvc": { "name": "e2e-ro-dst", "capacity": "1Gi", "accessModes": ["ReadWriteOnce"] } }
                }
            })),
        )
        .await
        .expect("create Restore against ReadOnly repo");
    // The Restore is ADMITTED and dispatched (reaches `Restoring` + builds a mover
    // Job) — proving a ReadOnly repository SERVES reads (it is not refused the way the
    // backup above was). We don't assert `Completed`: the template target PVC is
    // dynamically provisioned and may never bind in the e2e cluster (the existing
    // restore scenarios note the same), which is orthogonal to the ReadOnly behavior
    // under test.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = wait_for_job(&jobs, "e2e-ro-restore").await;
    assert_ne!(
        status_json(&restores, "e2e-ro-restore")
            .await
            .get("phase")
            .and_then(|p| p.as_str()),
        Some("Failed"),
        "a Restore against a ReadOnly repository must be served, not refused"
    );

    // Cleanup.
    let _ = jobs
        .delete("e2e-ro-restore", &DeleteParams::default())
        .await;
    let _ = restores
        .delete("e2e-ro-restore", &DeleteParams::default())
        .await;
    let _ = backups
        .delete("e2e-ro-backup", &DeleteParams::default())
        .await;
    let _ = policies
        .delete("e2e-ro-policy", &DeleteParams::default())
        .await;
    let _ = repos.delete(ro_repo, &DeleteParams::default()).await;
}

/// kstatus `Ready` (ADR-0005 §2): once a Repository is Ready and a SnapshotPolicy is
/// reconciled, the SnapshotPolicy carries a `Ready=True` condition AND a Succeeded
/// Snapshot does too — so `kubectl wait --for=condition=Ready` (and Flux/Argo health)
/// work. We assert the condition the way `kubectl wait` reads it.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn kstatus_ready_condition_present_for_wait() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_seed(
        &client,
        "e2e-ready-repo",
        "e2e-ready-policy",
        "e2e-ready-seed",
        "kstatus",
    )
    .await;

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // The SnapshotPolicy reaches Ready=True (its Repository is Ready, retention enforced).
    wait_condition(&policies, "e2e-ready-policy", "Ready", "True")
        .await
        .expect(
            "SnapshotPolicy must carry Ready=True so `kubectl wait --for=condition=Ready` works",
        );
    // A Succeeded Snapshot also carries Ready=True (kstatus on the data resource).
    wait_condition(&backups, "e2e-ready-seed", "Ready", "True")
        .await
        .expect("a Succeeded Snapshot must carry Ready=True");

    // Cleanup leaves the seed for reuse (E2E_NAMESPACE persists); nothing to delete.
}

/// Fixing a credential Secret IN PLACE un-sticks a terminally-`Failed`
/// Repository — with ZERO edits to the CR itself. This is the
/// `watch.rs::secret_to_repositories` mapper + the `terminal_gate_holds`
/// credential-version key (`status.resolvedCredentialVersion`) working
/// together: a Secret content edit bumps neither the repo's generation nor any
/// spec field, so on the buggy generation-only gate this test times out with
/// the repo parked `Failed` until the 30-minute heartbeat.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn fixed_credential_secret_unsticks_failed_repository() {
    use kopiur_e2e::{apply_secret, consts};
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("fixtures ready");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // The credential-version gate lives in the IN-PROCESS filesystem arm (no
    // `volume`): the controller itself connects at /repo. First make sure the
    // repo at /repo is initialized with the GOOD password (idempotent).
    let init = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": "e2e-rotate-init", "namespace": E2E_NAMESPACE },
        "spec": {
            "backend": { "filesystem": { "path": "/repo" } },
            "encryption": { "passwordSecretRef": { "name": CREDS_SECRET, "key": "KOPIA_PASSWORD" } },
            "create": { "enabled": true }
        }
    });
    let _ = repos.create(&PostParams::default(), &cr(init)).await;
    wait_phase(&repos, "e2e-rotate-init", "Ready")
        .await
        .expect("the /repo repository should initialize with the good password");

    // A dedicated Secret, seeded with the WRONG password.
    apply_secret(
        &client,
        E2E_NAMESPACE,
        "e2e-rotate-creds",
        &[("KOPIA_PASSWORD", consts::KOPIA_BADPW)],
    )
    .await
    .expect("seed the bad-password Secret");

    let repo = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": "e2e-rotate", "namespace": E2E_NAMESPACE },
        "spec": {
            "backend": { "filesystem": { "path": "/repo" } },
            "encryption": { "passwordSecretRef": { "name": "e2e-rotate-creds", "key": "KOPIA_PASSWORD" } },
            "create": { "enabled": false }
        }
    });
    let _ = repos.create(&PostParams::default(), &cr(repo)).await;
    wait_phase(&repos, "e2e-rotate", "Failed")
        .await
        .expect("the wrong password must park the repository Failed (terminal)");
    let s = status_json(&repos, "e2e-rotate").await;
    let recorded_version = s
        .get("resolvedCredentialVersion")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        !recorded_version.is_empty(),
        "a terminal credential failure must pin status.resolvedCredentialVersion \
         (the gate key); got {s}"
    );
    let generation_before = repos
        .get("e2e-rotate")
        .await
        .expect("get repo")
        .metadata
        .generation;

    // Fix the Secret IN PLACE — content-only; the CR is never touched.
    apply_secret(
        &client,
        E2E_NAMESPACE,
        "e2e-rotate-creds",
        &[("KOPIA_PASSWORD", consts::KOPIA_PASSWORD)],
    )
    .await
    .expect("fix the password Secret in place");

    // The Secret watch + credential-version gate must re-drive the repo to Ready
    // well inside the harness timeout (the buggy gate waits ~30 minutes).
    wait_phase(&repos, "e2e-rotate", "Ready")
        .await
        .expect("a FIXED credential Secret must un-stick the Failed repository");
    let after = repos.get("e2e-rotate").await.expect("get repo");
    assert_eq!(
        after.metadata.generation, generation_before,
        "recovery must come from the Secret watch, not a CR edit (generation changed!)"
    );
    let s = status_json(&repos, "e2e-rotate").await;
    assert_ne!(
        s.get("resolvedCredentialVersion").and_then(|v| v.as_str()),
        Some(recorded_version.as_str()),
        "the gate key must advance to the fixed Secret's resourceVersion; got {s}"
    );

    let _ = repos.delete("e2e-rotate", &DeleteParams::default()).await;
}

/// Index-blob health (ADR-0005 §13): a repository whose content-index blob count
/// crosses `spec.health.indexBlobWarnThreshold` must surface it non-blockingly —
/// `status.storageStats.indexBlobCount`, an `IndexBlobHealth=False` condition
/// (reason `TooManyIndexBlobs`), and a Kubernetes **Warning** event — while
/// staying `Ready`. This is the symptom side of the maintenance-not-compacting
/// problem that wedged a real cluster at 1448 index blobs.
///
/// We drive it deterministically: kopia adds an index blob per snapshot session
/// (no maintenance compacts them here), so a couple of seeded snapshots push the
/// count above a threshold of 1. We start the threshold high (healthy), seed the
/// snapshots, then lower it to 1 — the spec edit bumps `generation`, which
/// recycles the bootstrap Job (`bootstrap_recycle_due`) for a fresh count read,
/// and exercises the healthy→unhealthy transition that gates the one-shot event.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn too_many_index_blobs_warns_without_blocking_ready() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_repo(&client, "idxhealth").await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let events: Api<Event> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // Repository over the isolated `idxhealth` repo, with the warning threshold
    // set HIGH so it starts healthy (a freshly-created repo has ~1 index blob).
    let repo = "e2e-idx-repo";
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                repo,
                "idxhealth",
                serde_json::json!({ "health": { "indexBlobWarnThreshold": 100 } }),
            )),
        )
        .await
        .expect("create Repository with health threshold");
    wait_phase(&repos, repo, "Ready")
        .await
        .expect("repo should connect to Ready");

    // Seed two snapshots so the index-blob count climbs above 1 (one index blob
    // per snapshot session; nothing compacts them mid-test).
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                "e2e-idx-policy",
                "Repository",
                repo,
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create SnapshotPolicy");
    for snap in ["e2e-idx-snap-1", "e2e-idx-snap-2"] {
        backups
            .create(
                &PostParams::default(),
                &cr(snapshot_json(
                    E2E_NAMESPACE,
                    snap,
                    "e2e-idx-policy",
                    serde_json::json!({}),
                )),
            )
            .await
            .expect("create Snapshot");
        wait_phase(&backups, snap, "Succeeded")
            .await
            .expect("seed Snapshot Succeeded");
    }

    // Lower the threshold to 1. The spec edit bumps generation → recycles the
    // bootstrap Job → fresh index_blob_count read against the now-1 threshold.
    repos
        .patch(
            repo,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "spec": { "health": { "indexBlobWarnThreshold": 1 } }
            })),
        )
        .await
        .expect("patch threshold to 1");

    // The IndexBlobHealth condition flips False with reason TooManyIndexBlobs.
    let cond = wait_condition(&repos, repo, "IndexBlobHealth", "False")
        .await
        .expect("a repo over the index-blob threshold must surface IndexBlobHealth=False");
    assert_eq!(
        cond.get("reason").and_then(|r| r.as_str()),
        Some("TooManyIndexBlobs"),
        "the index-blob warning reason must be TooManyIndexBlobs"
    );

    // The observed count is stamped on status and is above the threshold.
    let s = status_json(&repos, repo).await;
    let count = s
        .get("storageStats")
        .and_then(|ss| ss.get("indexBlobCount"))
        .and_then(|c| c.as_i64())
        .unwrap_or(0);
    assert!(
        count >= 2,
        "status.storageStats.indexBlobCount must reflect the seeded snapshots (got {count}); status={s}"
    );

    // The repository stays Ready — the warning is informational, NOT an outage,
    // so GitOps health gates are not tripped.
    assert_eq!(
        s.get("phase").and_then(|p| p.as_str()),
        Some("Ready"),
        "too many index blobs must NOT take the repository out of Ready"
    );

    // A Warning event with the machine-readable reason is published on the repo.
    wait_until(
        "a TooManyIndexBlobs Warning event is published for the Repository",
        default_timeout(),
        poll_interval(),
        || async {
            let list = events.list(&ListParams::default()).await?;
            let found = list.items.iter().any(|e| {
                e.type_.as_deref() == Some("Warning")
                    && e.reason.as_deref() == Some("TooManyIndexBlobs")
                    && e.regarding.as_ref().and_then(|r| r.name.as_deref()) == Some(repo)
            });
            Ok(found.then_some(()))
        },
    )
    .await
    .expect("the index-blob-health Warning event must be published");

    // Cleanup: remove the snapshots (Retain → no kopia delete) and the repo.
    for snap in ["e2e-idx-snap-1", "e2e-idx-snap-2"] {
        let _ = backups.delete(snap, &DeleteParams::default()).await;
    }
    let _ = policies
        .delete("e2e-idx-policy", &DeleteParams::default())
        .await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
}
