//! e2e: `ClusterRepository.spec.execution.mode: Central`: the backup mover runs
//! in the OPERATOR namespace and reads a cross-namespace CSI snapshot restore.
//!
//! Three scenarios:
//!
//! 1. **Central backup**: a CSI PVC in a workload namespace that holds neither the
//!    repository credentials nor the repository volume is backed up by a mover Job
//!    in the operator namespace. Nothing runs in, and no Secret enters, the
//!    workload namespace; every transient object is reaped afterwards. The same
//!    scenario then restores, verifies and pins the snapshot, all from the
//!    operator namespace.
//! 2. **Refusal, then a clone**: a policy the central path cannot serve
//!    (`copyMethod: Direct`) fails with the reason, instead of quietly running a
//!    mover beside the source. A `copyMethod: Clone` policy with a
//!    `sourcePathOverride` then runs centrally: the override is what kopia
//!    records, never where the volume is mounted.
//!
//! 3. **Central group**: a `groupBy: VolumeGroupSnapshot` policy over two PVCs.
//!    One shared capture in the workload namespace, each member staged and
//!    backed up in the operator namespace.
//!
//! Needs the alpha `CrossNamespaceVolumeDataSource` feature on kube-apiserver and
//! kube-controller-manager (`crates/e2e/kind-config.yaml`) and on the CSI
//! provisioner, plus the `ReferenceGrant` CRD (`manifests/csi-snapshot`).
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Api, Client, ResourceExt};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
use k8s_openapi::api::rbac::v1::RoleBinding;
use kopiur_api::{ClusterRepository, Restore, Snapshot, SnapshotPolicy};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, default_timeout, ensure_namespace, poll_interval, wait_until,
};

/// The workload namespace. Deliberately bare: no credentials Secret and no
/// repository PVC, so an in-namespace mover could not run here at all.
const WORKLOAD_NS: &str = "kopiur-e2e-central";
/// What the seed pod writes into the source volume.
const MARKER: &str = "kopiur-central-marker";

fn dynamic_api(
    client: &Client,
    ns: &str,
    group: &str,
    version: &str,
    kind: &str,
) -> Api<DynamicObject> {
    let plural = format!("{}s", kind.to_lowercase());
    let ar =
        ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(group, version, kind), &plural);
    Api::namespaced_with(client.clone(), ns, &ar)
}

fn volume_snapshots(client: &Client, ns: &str) -> Api<DynamicObject> {
    dynamic_api(
        client,
        ns,
        "snapshot.storage.k8s.io",
        "v1",
        "VolumeSnapshot",
    )
}

fn reference_grants(client: &Client, ns: &str) -> Api<DynamicObject> {
    dynamic_api(
        client,
        ns,
        "gateway.networking.k8s.io",
        "v1beta1",
        "ReferenceGrant",
    )
}

fn volume_group_snapshots(client: &Client, ns: &str) -> Api<DynamicObject> {
    dynamic_api(
        client,
        ns,
        "groupsnapshot.storage.k8s.io",
        "v1",
        "VolumeGroupSnapshot",
    )
}

/// A CSI PVC in `ns` holding `/data/marker.txt`, bound.
async fn csi_source_with_marker(client: &Client, ns: &str, name: &str) {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let seed = format!("{name}-seed");
    // A retry of this test finds the previous attempt's objects; start clean.
    kopiur_e2e::pvc::delete_pvc_and_consumers(client, ns, name)
        .await
        .expect("clear a previous source PVC");
    pvcs.create(
        &PostParams::default(),
        &cr(serde_json::json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": { "name": name, "namespace": ns },
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
            "metadata": { "name": seed, "namespace": ns },
            "spec": {
                "restartPolicy": "Never",
                "containers": [{
                    "name": "seed", "image": kopiur_e2e::consts::BUSYBOX_IMAGE,
                    "imagePullPolicy": "IfNotPresent",
                    "command": ["sh", "-c", format!("echo {MARKER} > /data/marker.txt")],
                    "volumeMounts": [{ "name": "d", "mountPath": "/data" }],
                }],
                "volumes": [{ "name": "d", "persistentVolumeClaim": { "claimName": name } }],
            },
        })),
    )
    .await
    .expect("create seed pod");
    kopiur_e2e::wait::pod_succeeded(client, ns, &seed)
        .await
        .expect("the seed pod should write the marker");
    // The seed pod is done; drop it so the workload namespace holds no pod at
    // all when the backup runs (the assertion below counts them).
    let _ = pods.delete(&seed, &DeleteParams::default()).await;
    wait_until(
        "seed pod gone",
        default_timeout(),
        poll_interval(),
        || async { Ok(pods.get_opt(&seed).await?.is_none().then_some(())) },
    )
    .await
    .expect("the seed pod should be deleted");
}

/// A `ClusterRepository` with `execution.mode: Central` over its own repo dir.
/// Its credentials Secret and its filesystem PVC are both in the operator
/// namespace: the only place a central mover looks.
async fn central_repository(client: &Client, name: &str, subpath: &str) -> Api<ClusterRepository> {
    ensure_repo(client, subpath).await;
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let _ = crepos.delete(name, &DeleteParams::default()).await;
    wait_until(
        "previous ClusterRepository gone",
        default_timeout(),
        poll_interval(),
        || async { Ok(crepos.get_opt(name).await?.is_none().then_some(())) },
    )
    .await
    .expect("a previous ClusterRepository must clear");
    crepos
        .create(
            &PostParams::default(),
            &cr(cluster_repository_json(
                name,
                subpath,
                serde_json::json!({ "execution": { "mode": "Central" } }),
            )),
        )
        .await
        .expect("create ClusterRepository");
    wait_phase(&crepos, name, "Ready")
        .await
        .expect("ClusterRepository should reach Ready");
    crepos
}

#[tokio::test]
#[ignore = "requires the e2e harness + the CSI snapshot stack (mise run //crates/e2e:snapshot-stack)"]
async fn central_backup_runs_in_the_operator_namespace_and_cleans_up() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }
    ensure_namespace(&client, WORKLOAD_NS)
        .await
        .expect("workload namespace");

    let src = "e2e-central-src";
    let policy = "e2e-central-policy";
    let backup = "e2e-central-backup";
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), WORKLOAD_NS);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), WORKLOAD_NS);
    let _ = backups.delete(backup, &DeleteParams::default()).await;
    wait_until(
        "previous Snapshot gone",
        default_timeout(),
        poll_interval(),
        || async { Ok(backups.get_opt(backup).await?.is_none().then_some(())) },
    )
    .await
    .expect("a previous Snapshot must clear");
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    // A retry of this test finds the previous attempt's restore objects.
    let stale_restores: Api<Restore> = Api::namespaced(client.clone(), WORKLOAD_NS);
    let _ = stale_restores
        .delete("e2e-central-restore", &DeleteParams::default())
        .await;
    wait_until(
        "previous Restore gone",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(stale_restores
                .get_opt("e2e-central-restore")
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("a previous Restore must clear");
    kopiur_e2e::pvc::delete_pvc_and_consumers(&client, WORKLOAD_NS, "e2e-central-restored")
        .await
        .expect("clear a previous restored PVC");
    let _ = stale_restores
        .delete("e2e-central-restore-new", &DeleteParams::default())
        .await;
    wait_until(
        "previous target.pvc Restore gone",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(stale_restores
                .get_opt("e2e-central-restore-new")
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("a previous Restore must clear");
    kopiur_e2e::pvc::delete_pvc_and_consumers(&client, WORKLOAD_NS, "e2e-central-restored-new")
        .await
        .expect("clear a previous restored PVC");

    csi_source_with_marker(&client, WORKLOAD_NS, src).await;
    let crepos = central_repository(&client, "e2e-central-repo", "central").await;

    // The premise: an in-namespace mover would have nothing to load here.
    let workload_secrets: Api<Secret> = Api::namespaced(client.clone(), WORKLOAD_NS);
    assert!(
        workload_secrets
            .get_opt(CREDS_SECRET)
            .await
            .expect("read secret")
            .is_none(),
        "the workload namespace must not hold the repository credentials"
    );

    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            WORKLOAD_NS,
            policy,
            "ClusterRepository",
            "e2e-central-repo",
            serde_json::json!({
                "copyMethod": "Snapshot",
                "sources": [ { "pvc": { "name": src } } ],
            }),
        )),
        "create SnapshotPolicy",
    )
    .await;
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                WORKLOAD_NS,
                backup,
                policy,
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Snapshot");

    wait_phase(&backups, backup, "Succeeded")
        .await
        .expect("the centrally executed Snapshot should Succeed");
    let status = status_json(&backups, backup).await;

    // A real kopia snapshot that holds the marker file: not an empty volume,
    // which is what a dropped cross-namespace dataSourceRef would have produced.
    let snap_id = status["snapshot"]["kopiaSnapshotID"].as_str().unwrap_or("");
    assert!(!snap_id.is_empty(), "no kopia snapshot id: {status}");
    // `sizeBytes` is the snapshot's total size, so it holds on a retry of this
    // test too, when kopia finds the marker already in the repository.
    let stats = &status["stats"];
    let files: i64 = ["filesNew", "filesModified", "filesUnchanged"]
        .iter()
        .map(|k| stats[*k].as_i64().unwrap_or(0))
        .sum();
    assert!(
        files >= 1,
        "the snapshot must contain the marker file: {status}"
    );
    assert!(
        stats["sizeBytes"].as_i64().unwrap_or(0) >= MARKER.len() as i64,
        "the snapshot must contain the marker's bytes: {status}"
    );

    // Verification and a pin read only the repository. Start both now, so they
    // run while the rest of the scenario does; they are checked further down.
    policies
        .patch(
            policy,
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({
                "spec": { "verification": {
                    "quick": { "schedule": { "cron": "* * * * *" } }
                } }
            })),
        )
        .await
        .expect("patch verification onto the SnapshotPolicy");
    backups
        .patch(
            backup,
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({ "spec": { "pin": true } })),
        )
        .await
        .expect("pin the Snapshot");
    let policy_uid = policies
        .get(policy)
        .await
        .expect("get SnapshotPolicy")
        .uid()
        .expect("policy uid");

    // The run pinned where its Job lives: the operator namespace.
    assert_eq!(
        status["job"]["namespace"].as_str(),
        Some(E2E_NAMESPACE),
        "status.job.namespace must name the operator namespace: {status}"
    );
    let job_name = status["job"]["name"]
        .as_str()
        .expect("status.job.name")
        .to_string();
    let uid = backups
        .get(backup)
        .await
        .expect("get Snapshot")
        .uid()
        .unwrap();

    // The mover Job is there, marked for this Snapshot, with no owner reference
    // (a cross-namespace one would have had it garbage collected).
    let central_jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let job = central_jobs
        .get(&job_name)
        .await
        .expect("the mover Job must exist in the operator namespace");
    assert_eq!(
        job.labels().get("kopiur.home-operations.com/snapshot-uid"),
        Some(&uid),
        "the central Job must carry the Snapshot's UID"
    );
    assert!(
        job.owner_references().is_empty(),
        "a central Job must not carry an owner reference"
    );

    // Nothing ran in the workload namespace, and no Secret was copied into it.
    let workload_jobs: Api<Job> = Api::namespaced(client.clone(), WORKLOAD_NS);
    assert!(
        workload_jobs
            .list(&ListParams::default())
            .await
            .expect("list jobs")
            .items
            .is_empty(),
        "no Job may be created in the workload namespace"
    );
    let workload_pods: Api<Pod> = Api::namespaced(client.clone(), WORKLOAD_NS);
    assert!(
        workload_pods
            .list(&ListParams::default())
            .await
            .expect("list pods")
            .items
            .is_empty(),
        "no pod may run in the workload namespace"
    );
    let copied: Vec<String> = workload_secrets
        .list(&ListParams::default())
        .await
        .expect("list secrets")
        .items
        .iter()
        .map(ResourceExt::name_any)
        .collect();
    assert!(
        copied.is_empty(),
        "no Secret may be projected into the workload namespace, found {copied:?}"
    );

    // The mover's only foothold there: a RoleBinding to the status-only role,
    // for a ServiceAccount that lives in the operator namespace.
    let bindings: Api<RoleBinding> = Api::namespaced(client.clone(), WORKLOAD_NS);
    let central_binding = bindings
        .list(&ListParams::default())
        .await
        .expect("list rolebindings")
        .items
        .into_iter()
        .find(|rb| rb.role_ref.name.ends_with("-central-mover"))
        .expect("a RoleBinding to the central mover role");
    let subject = &central_binding.subjects.as_ref().expect("subjects")[0];
    assert_eq!(subject.namespace.as_deref(), Some(E2E_NAMESPACE));

    // Every transient object is reaped: the staged PVC in the operator namespace,
    // and the VolumeSnapshot + ReferenceGrant beside the source.
    let central_pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let staged_pvc = status["staged"]["pvcName"]
        .as_str()
        .expect("status.staged.pvcName")
        .to_string();
    assert_eq!(staged_pvc, format!("{job_name}-src"));
    wait_until(
        "central staged PVC reaped",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(central_pvcs
                .get_opt(&staged_pvc)
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("the staged PVC should be cleaned up after the backup succeeds");
    let snapshots = volume_snapshots(&client, WORKLOAD_NS);
    wait_until(
        "VolumeSnapshot reaped",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(snapshots
                .list(&ListParams::default())
                .await?
                .items
                .is_empty()
                .then_some(()))
        },
    )
    .await
    .expect("the VolumeSnapshot should be cleaned up");
    let grants = reference_grants(&client, WORKLOAD_NS);
    wait_until(
        "ReferenceGrant reaped",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(grants
                .list(&ListParams::default())
                .await?
                .items
                .is_empty()
                .then_some(()))
        },
    )
    .await
    .expect("the ReferenceGrant should be cleaned up");

    // --- Restore, also from outside the workload namespace -------------------
    //
    // A populator Restore: the mover fills a prime PVC in the OPERATOR
    // namespace, and its volume is then handed to the PVC that claims the
    // Restore. The workload namespace still holds no credentials, so a mover
    // there could not have read the repository at all.
    let restore = "e2e-central-restore";
    let claim = "e2e-central-restored";
    let reader = "e2e-central-restored-reader";
    let restores: Api<Restore> = Api::namespaced(client.clone(), WORKLOAD_NS);
    let workload_pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), WORKLOAD_NS);
    restores
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Restore",
                "metadata": { "name": restore, "namespace": WORKLOAD_NS },
                "spec": {
                    "repository": { "kind": "ClusterRepository", "name": "e2e-central-repo" },
                    "source": { "snapshotRef": { "name": backup } },
                    "target": { "populator": {} },
                },
            })),
        )
        .await
        .expect("create Restore");
    workload_pvcs
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                "metadata": { "name": claim, "namespace": WORKLOAD_NS },
                "spec": {
                    "accessModes": ["ReadWriteOnce"],
                    "storageClassName": CSI_STORAGE_CLASS,
                    "resources": { "requests": { "storage": "64Mi" } },
                    "dataSourceRef": {
                        "apiGroup": "kopiur.home-operations.com",
                        "kind": "Restore",
                        "name": restore,
                    },
                },
            })),
        )
        .await
        .expect("create the claiming PVC");

    wait_phase(&restores, restore, "Completed")
        .await
        .expect("the centrally executed Restore should Complete");
    let restore_status = status_json(&restores, restore).await;
    assert_eq!(
        restore_status["execution"]["namespace"].as_str(),
        Some(E2E_NAMESPACE),
        "status.execution.namespace must name the operator namespace: {restore_status}"
    );
    let restore_obj = restores.get(restore).await.expect("get Restore");
    assert!(
        restore_obj
            .finalizers()
            .iter()
            .any(|f| f == kopiur_api::consts::RESTORE_CENTRAL_CLEANUP_FINALIZER),
        "a central Restore must carry the cleanup finalizer"
    );
    let restore_uid = restore_obj.uid().unwrap();

    // The restore mover never ran here, and still no Secret was copied in.
    assert!(
        workload_jobs
            .list(&ListParams::default())
            .await
            .expect("list jobs")
            .items
            .is_empty(),
        "no restore Job may be created in the workload namespace"
    );
    assert!(
        workload_secrets
            .list(&ListParams::default())
            .await
            .expect("list secrets")
            .items
            .is_empty(),
        "no Secret may be projected into the workload namespace for the restore"
    );

    // The claiming PVC holds the data that was backed up.
    let workload_pod_api: Api<Pod> = Api::namespaced(client.clone(), WORKLOAD_NS);
    workload_pod_api
        .create(
            &PostParams::default(),
            &kopiur_e2e::builders::one_shot_pod(
                WORKLOAD_NS,
                reader,
                &[
                    "sh",
                    "-c",
                    &format!("test \"$(cat /mnt/marker.txt)\" = '{MARKER}'"),
                ],
                &[(claim, "/mnt")],
            ),
        )
        .await
        .expect("create the reader pod");
    kopiur_e2e::wait::pod_succeeded(&client, WORKLOAD_NS, reader)
        .await
        .expect("the restored PVC must hold the marker file");

    // The prime PVC and the mover Job were in the operator namespace, marked
    // with the Restore's UID, and are gone once the volume is handed over.
    let restore_selector = format!("kopiur.home-operations.com/restore-uid={restore_uid}");
    let by_restore = ListParams::default().labels(&restore_selector);
    wait_until(
        "central restore artifacts reaped",
        default_timeout(),
        poll_interval(),
        || async {
            let jobs = central_jobs.list(&by_restore).await?.items.len();
            let primes = central_pvcs.list(&by_restore).await?.items.len();
            Ok((jobs == 0 && primes == 0).then_some(()))
        },
    )
    .await
    .expect("the central prime PVC and mover Job should be cleaned up");

    // Deleting the Restore runs its finalizer and releases it.
    let _ = workload_pod_api
        .delete(reader, &DeleteParams::default())
        .await;
    restores
        .delete(restore, &DeleteParams::default())
        .await
        .expect("delete Restore");
    wait_until(
        "Restore gone",
        default_timeout(),
        poll_interval(),
        || async { Ok(restores.get_opt(restore).await?.is_none().then_some(())) },
    )
    .await
    .expect("the Restore's cleanup finalizer should release it");
    kopiur_e2e::pvc::delete_pvc_and_consumers(&client, WORKLOAD_NS, claim)
        .await
        .expect("delete the restored PVC");

    // --- Restore into a NEW named PVC (`target.pvc`) --------------------------
    //
    // Nobody authors a claim here. The operator creates the target PVC as a
    // claim on the Restore, and the same handover fills it. The class binds
    // late (WaitForFirstConsumer) and no pod uses the PVC yet: the central
    // mover pod is the first consumer of the temporary volume, exactly as an
    // in-namespace `target.pvc` restore's mover is of the target.
    let new_restore = "e2e-central-restore-new";
    let new_pvc = "e2e-central-restored-new";
    let new_reader = "e2e-central-restored-new-reader";
    restores
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Restore",
                "metadata": { "name": new_restore, "namespace": WORKLOAD_NS },
                "spec": {
                    "repository": { "kind": "ClusterRepository", "name": "e2e-central-repo" },
                    "source": { "snapshotRef": { "name": backup } },
                    "target": { "pvc": {
                        "name": new_pvc,
                        "storageClassName": CSI_STORAGE_CLASS_WFFC,
                        "capacity": "64Mi",
                    } },
                },
            })),
        )
        .await
        .expect("create the target.pvc Restore");
    wait_phase(&restores, new_restore, "Completed")
        .await
        .expect("the central target.pvc Restore should Complete");
    let new_status = status_json(&restores, new_restore).await;
    assert_eq!(
        new_status["execution"]["namespace"].as_str(),
        Some(E2E_NAMESPACE),
        "a target.pvc restore of a Central repository must run centrally: {new_status}"
    );
    let created = workload_pvcs
        .get(new_pvc)
        .await
        .expect("the operator creates the target PVC");
    let claim_ref = created
        .spec
        .as_ref()
        .and_then(|s| s.data_source_ref.as_ref())
        .expect("the target PVC is created as a claim on the Restore");
    assert_eq!(claim_ref.kind, "Restore");
    assert_eq!(claim_ref.name, new_restore);
    assert!(
        workload_jobs
            .list(&ListParams::default())
            .await
            .expect("list jobs")
            .items
            .is_empty(),
        "no restore Job may be created in the workload namespace for target.pvc"
    );
    assert!(
        workload_secrets
            .list(&ListParams::default())
            .await
            .expect("list secrets")
            .items
            .is_empty(),
        "no Secret may be projected into the workload namespace for target.pvc"
    );
    workload_pod_api
        .create(
            &PostParams::default(),
            &kopiur_e2e::builders::one_shot_pod(
                WORKLOAD_NS,
                new_reader,
                &[
                    "sh",
                    "-c",
                    &format!("test \"$(cat /mnt/marker.txt)\" = '{MARKER}'"),
                ],
                &[(new_pvc, "/mnt")],
            ),
        )
        .await
        .expect("create the reader pod");
    kopiur_e2e::wait::pod_succeeded(&client, WORKLOAD_NS, new_reader)
        .await
        .expect("the new PVC must hold the marker file");
    let _ = workload_pod_api
        .delete(new_reader, &DeleteParams::default())
        .await;
    restores
        .delete(new_restore, &DeleteParams::default())
        .await
        .expect("delete the target.pvc Restore");
    wait_until(
        "target.pvc Restore gone",
        default_timeout(),
        poll_interval(),
        || async { Ok(restores.get_opt(new_restore).await?.is_none().then_some(())) },
    )
    .await
    .expect("the Restore's cleanup finalizer should release it");
    // The restored data outlives its Restore.
    assert!(
        workload_pvcs
            .get_opt(new_pvc)
            .await
            .expect("read the restored PVC")
            .is_some(),
        "deleting the Restore must not delete the PVC it restored"
    );
    kopiur_e2e::pvc::delete_pvc_and_consumers(&client, WORKLOAD_NS, new_pvc)
        .await
        .expect("delete the restored PVC");

    // --- Verification and pin, also from the operator namespace --------------
    // The workload namespace holds no credentials, so a verify or pin mover
    // there could not even start: these two results prove the central site.
    wait_until(
        "SnapshotPolicy.status.lastVerified is stamped by a central quick verify",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&policies, policy).await;
            Ok(s.get("lastVerified")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|_| ()))
        },
    )
    .await
    .expect("a central quick verify must stamp status.lastVerified");
    let policy_selector = format!("kopiur.home-operations.com/policy-uid={policy_uid}");
    let by_policy = ListParams::default().labels(&policy_selector);
    let verify_jobs = central_jobs
        .list(&by_policy)
        .await
        .expect("list verify Jobs");
    assert!(
        !verify_jobs.items.is_empty(),
        "the verify Job must be in the operator namespace, labelled with the policy UID"
    );
    for job in &verify_jobs.items {
        assert!(
            job.metadata.owner_references.is_none(),
            "a central verify Job must carry no owner reference: {:?}",
            job.metadata.owner_references
        );
    }
    wait_until(
        "Snapshot.status.pinned is recorded by a central pin",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&backups, backup).await;
            Ok((s.get("pinned").and_then(|v| v.as_bool()) == Some(true)).then_some(()))
        },
    )
    .await
    .expect("a central pin mover must record status.pinned");
    let stray: Vec<String> = workload_jobs
        .list(&ListParams::default())
        .await
        .expect("list workload Jobs")
        .items
        .iter()
        .map(|j| j.name_any())
        .collect();
    assert!(
        stray.is_empty(),
        "no verify or pin Job may run in the workload namespace: {stray:?}"
    );
    assert!(
        workload_secrets
            .list(&ListParams::default())
            .await
            .expect("list workload Secrets")
            .items
            .is_empty(),
        "verification and the pin must not copy a Secret into the workload namespace"
    );

    // Deleting the Snapshot deletes the central Job: nothing else would, since
    // it has no owner reference.
    backups
        .delete(backup, &DeleteParams::default())
        .await
        .expect("delete Snapshot");
    wait_until(
        "central Job deleted with its Snapshot",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(central_jobs
                .get_opt(&job_name)
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("the central mover Job should be deleted with its Snapshot");

    // Deleting the policy deletes its central verify Jobs, for the same reason.
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    wait_until(
        "central verify Jobs deleted with their SnapshotPolicy",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(central_jobs
                .list(&by_policy)
                .await?
                .items
                .is_empty()
                .then_some(()))
        },
    )
    .await
    .expect("the central verify Jobs should be deleted with their SnapshotPolicy");
    let _ = crepos
        .delete("e2e-central-repo", &DeleteParams::default())
        .await;
    kopiur_e2e::pvc::delete_pvc_and_consumers(&client, WORKLOAD_NS, src)
        .await
        .expect("delete the source PVC");
}

#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn central_repository_refuses_a_source_it_cannot_carry_across_namespaces() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_namespace(&client, WORKLOAD_NS)
        .await
        .expect("workload namespace");

    let policy = "e2e-central-direct-policy";
    let backup = "e2e-central-direct-backup";
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), WORKLOAD_NS);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), WORKLOAD_NS);
    let _ = backups.delete(backup, &DeleteParams::default()).await;
    wait_until(
        "previous Snapshot gone",
        default_timeout(),
        poll_interval(),
        || async { Ok(backups.get_opt(backup).await?.is_none().then_some(())) },
    )
    .await
    .expect("a previous Snapshot must clear");
    let crepos = central_repository(&client, "e2e-central-refuse-repo", "central-refuse").await;

    // `copyMethod: Direct` mounts the live source, which only a pod in the
    // workload namespace can do.
    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            WORKLOAD_NS,
            policy,
            "ClusterRepository",
            "e2e-central-refuse-repo",
            serde_json::json!({
                "copyMethod": "Direct",
                "sources": [ { "pvc": { "name": "does-not-matter" } } ],
            }),
        )),
        "create SnapshotPolicy",
    )
    .await;
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                WORKLOAD_NS,
                backup,
                policy,
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Snapshot");

    let condition = wait_condition(&backups, backup, "SourceStaged", "False")
        .await
        .expect("the Snapshot should be refused");
    assert_eq!(
        condition["reason"].as_str(),
        Some("CentralExecutionUnsupported"),
        "{condition}"
    );
    let message = condition["message"].as_str().unwrap_or("");
    assert!(message.contains("copyMethod: Direct"), "{message}");
    wait_phase(&backups, backup, "Failed")
        .await
        .expect("a refused Snapshot is Failed");

    // Refused means refused: no mover anywhere, and no pin to a central site.
    let status = status_json(&backups, backup).await;
    assert!(
        status.get("job").is_none(),
        "nothing was launched: {status}"
    );
    let workload_jobs: Api<Job> = Api::namespaced(client.clone(), WORKLOAD_NS);
    assert!(
        workload_jobs
            .list(&ListParams::default())
            .await
            .expect("list jobs")
            .items
            .is_empty(),
        "a Central repository must never fall back to a mover in the workload namespace"
    );

    // --- A CSI clone, with a `sourcePathOverride` ----------------------------
    // A PVC also crosses namespaces as a CSI clone. And a `sourcePathOverride`
    // is honoured for what kopia RECORDS, without ever becoming a mount path:
    // a central mover pod holds the repository credentials, and a volume
    // mounted over `/usr/local/bin` would replace the mover binary with
    // whatever the tenant put on the volume.
    let clone_src = "e2e-central-clone-src";
    let override_policy = "e2e-central-override-policy";
    let override_backup = "e2e-central-override-backup";
    let _ = backups
        .delete(override_backup, &DeleteParams::default())
        .await;
    wait_until(
        "previous override Snapshot gone",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(backups
                .get_opt(override_backup)
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("a previous Snapshot must clear");
    let _ = policies
        .delete(override_policy, &DeleteParams::default())
        .await;
    csi_source_with_marker(&client, WORKLOAD_NS, clone_src).await;
    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            WORKLOAD_NS,
            override_policy,
            "ClusterRepository",
            "e2e-central-refuse-repo",
            serde_json::json!({
                "copyMethod": "Clone",
                "sources": [ {
                    "pvc": { "name": clone_src },
                    "sourcePathOverride": "/usr/local/bin",
                } ],
            }),
        )),
        "create SnapshotPolicy",
    )
    .await;
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                WORKLOAD_NS,
                override_backup,
                override_policy,
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Snapshot");
    wait_phase(&backups, override_backup, "Succeeded")
        .await
        .expect("the central clone backup should Succeed");
    let status = status_json(&backups, override_backup).await;
    assert_eq!(
        status["job"]["namespace"].as_str(),
        Some(E2E_NAMESPACE),
        "the clone backup must run in the operator namespace: {status}"
    );
    let files: i64 = ["filesNew", "filesModified", "filesUnchanged"]
        .iter()
        .map(|k| status["stats"][*k].as_i64().unwrap_or(0))
        .sum();
    assert!(
        files >= 1,
        "the clone must hold the marker file, not an empty volume: {status}"
    );
    assert!(
        status["staged"]["volumeSnapshotName"].is_null(),
        "a clone cuts no VolumeSnapshot: {status}"
    );

    // The mover pod mounted the source at the fixed central path, and kopia
    // recorded the snapshot under the overridden one.
    let central_jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let job = central_jobs
        .get(status["job"]["name"].as_str().expect("status.job.name"))
        .await
        .expect("the central mover Job");
    let container = job
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|p| p.containers.first())
        .expect("the mover container");
    let mounts: Vec<&str> = container
        .volume_mounts
        .iter()
        .flatten()
        .map(|m| m.mount_path.as_str())
        .collect();
    assert!(
        mounts.contains(&"/source"),
        "the source must be mounted at the fixed central path: {mounts:?}"
    );
    assert!(
        !mounts.iter().any(|m| m.starts_with("/usr")),
        "a tenant-chosen path must never be a mount path in a central mover: {mounts:?}"
    );
    let work_spec: serde_json::Value = container
        .env
        .iter()
        .flatten()
        .find(|e| e.name == "KOPIUR_WORK_SPEC")
        .and_then(|e| e.value.as_deref())
        .map(|raw| serde_json::from_str(raw).expect("the work spec parses"))
        .expect("the Job carries the inline work spec");
    assert_eq!(
        work_spec["operation"]["snapshot"]["sourcePath"].as_str(),
        Some("/source"),
        "{work_spec}"
    );
    assert_eq!(
        work_spec["identity"]["sourcePath"].as_str(),
        Some("/usr/local/bin"),
        "kopia must record the overridden path, as an in-namespace run does: {work_spec}"
    );

    // Cleanup after the clone: the staged PVC and the grant are gone. (The
    // provisioner's `cloning-protection` finalizer stays on the source PVC, as
    // after any CSI clone. The provisioner removes it when that PVC is deleted,
    // which the end of this test does.)
    let override_uid = backups
        .get(override_backup)
        .await
        .expect("get Snapshot")
        .uid()
        .unwrap();
    let central_pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let grants = reference_grants(&client, WORKLOAD_NS);
    let by_snapshot = ListParams::default().labels(&format!(
        "kopiur.home-operations.com/snapshot-uid={override_uid}"
    ));
    wait_until(
        "central clone artifacts reaped",
        default_timeout(),
        poll_interval(),
        || async {
            let staged = central_pvcs.list(&by_snapshot).await?.items.len();
            let granted = grants.list(&ListParams::default()).await?.items.len();
            Ok((staged == 0 && granted == 0).then_some(()))
        },
    )
    .await
    .expect("the staged clone and its ReferenceGrant should go");

    let _ = backups
        .delete(override_backup, &DeleteParams::default())
        .await;
    let _ = policies
        .delete(override_policy, &DeleteParams::default())
        .await;
    let _ = backups.delete(backup, &DeleteParams::default()).await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    wait_until(
        "the clone Snapshot gone",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(backups
                .get_opt(override_backup)
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("the clone Snapshot should be deleted");
    let _ = crepos
        .delete("e2e-central-refuse-repo", &DeleteParams::default())
        .await;
    kopiur_e2e::pvc::delete_pvc_and_consumers(&client, WORKLOAD_NS, clone_src)
        .await
        .expect("delete the clone source PVC");
}

/// A consistency group under central execution: ONE `VolumeGroupSnapshot` in the
/// workload namespace captures both PVCs, and each member is staged and backed
/// up in the operator namespace from its own member snapshot. Afterwards the
/// group, its member snapshots (released from the CSI provisioner's stale
/// source-protection finalizer) and every central object are gone.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn central_group_members_are_staged_in_the_operator_namespace_from_one_capture() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }
    ensure_namespace(&client, WORKLOAD_NS)
        .await
        .expect("workload namespace");

    let sources = ["e2e-central-grp-a", "e2e-central-grp-b"];
    let policy = "e2e-central-group-policy";
    let schedule = "e2e-central-group-schedule";
    let label_value = "central-group";
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), WORKLOAD_NS);
    let schedules: Api<kopiur_api::SnapshotSchedule> = Api::namespaced(client.clone(), WORKLOAD_NS);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), WORKLOAD_NS);
    let by_policy =
        ListParams::default().labels(&format!("kopiur.home-operations.com/config={policy}"));

    // A retry of this test finds the previous attempt's objects; start clean.
    let _ = schedules.delete(schedule, &DeleteParams::default()).await;
    for child in backups
        .list(&by_policy)
        .await
        .expect("list Snapshots")
        .items
    {
        let _ = backups
            .delete(&child.name_any(), &DeleteParams::default())
            .await;
    }
    wait_until(
        "previous grouped Snapshots gone",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(backups
                .list(&by_policy)
                .await?
                .items
                .is_empty()
                .then_some(()))
        },
    )
    .await
    .expect("previous grouped Snapshots must clear");
    let _ = policies.delete(policy, &DeleteParams::default()).await;

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), WORKLOAD_NS);
    for src in sources {
        csi_source_with_marker(&client, WORKLOAD_NS, src).await;
        pvcs.patch(
            src,
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({
                "metadata": { "labels": { BACKUP_LABEL_KEY: label_value } }
            })),
        )
        .await
        .expect("label the source PVC");
    }
    let crepos = central_repository(&client, "e2e-central-repo", "central").await;

    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            WORKLOAD_NS,
            policy,
            "ClusterRepository",
            "e2e-central-repo",
            serde_json::json!({
                "sources": [ {
                    "pvcSelector": {
                        "labelSelector": { "matchLabels": { BACKUP_LABEL_KEY: label_value } }
                    }
                } ],
                "groupBy": "VolumeGroupSnapshot",
                "copyMethod": "Snapshot",
                "identity": { "username": "central-grouped", "hostname": "e2e" }
            }),
        )),
        "create grouped SnapshotPolicy",
    )
    .await;
    create_idempotent(
        &schedules,
        &cr(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotSchedule",
            "metadata": { "name": schedule, "namespace": WORKLOAD_NS },
            "spec": {
                "policyRef": { "name": policy },
                "schedule": { "cron": "0 3 * * *", "runOnCreate": true }
            }
        })),
        "create SnapshotSchedule",
    )
    .await;

    wait_until(
        "both central group members Succeeded",
        default_timeout(),
        poll_interval(),
        || async {
            let kids = backups.list(&by_policy).await?.items;
            let done = kids
                .iter()
                .filter(|k| {
                    k.status.as_ref().and_then(|s| s.phase.as_ref())
                        == Some(&kopiur_api::SnapshotPhase::Succeeded)
                })
                .count();
            Ok((kids.len() == 2 && done == 2).then_some(()))
        },
    )
    .await
    .expect("both members of a central group capture should succeed");

    let children = backups
        .list(&by_policy)
        .await
        .expect("list Snapshots")
        .items;
    let mut groups = std::collections::BTreeSet::new();
    let mut uids = Vec::new();
    for child in &children {
        let name = child.name_any();
        let pinned = child
            .spec
            .source
            .as_ref()
            .and_then(|s| s.group.as_ref())
            .unwrap_or_else(|| panic!("child {name} must pin the shared group"));
        groups.insert(pinned.volume_group_snapshot_name.clone());
        let status = status_json(&backups, &name).await;
        assert_eq!(
            status["job"]["namespace"].as_str(),
            Some(E2E_NAMESPACE),
            "member {name} must run in the operator namespace: {status}"
        );
        let files: i64 = ["filesNew", "filesModified", "filesUnchanged"]
            .iter()
            .map(|k| status["stats"][*k].as_i64().unwrap_or(0))
            .sum();
        assert!(files >= 1, "member {name} must hold its marker: {status}");
        uids.push(child.uid().expect("uid"));
    }
    assert_eq!(
        groups.len(),
        1,
        "every member must pin the SAME group object: {groups:?}"
    );

    // Nothing ran in the workload namespace, and no Secret was copied into it.
    let workload_jobs: Api<Job> = Api::namespaced(client.clone(), WORKLOAD_NS);
    let workload_secrets: Api<Secret> = Api::namespaced(client.clone(), WORKLOAD_NS);
    assert!(
        workload_jobs
            .list(&ListParams::default())
            .await
            .expect("list jobs")
            .items
            .is_empty(),
        "no group member may run a Job in the workload namespace"
    );
    assert!(
        workload_secrets
            .list(&ListParams::default())
            .await
            .expect("list secrets")
            .items
            .is_empty(),
        "no Secret may be projected into the workload namespace"
    );

    // Everything transient is reaped: the shared group, its member snapshots
    // (which the CSI provisioner would otherwise hold with its stale
    // finalizer), the grants, and each member's staged PVC.
    let vgs = volume_group_snapshots(&client, WORKLOAD_NS);
    let vs = volume_snapshots(&client, WORKLOAD_NS);
    let grants = reference_grants(&client, WORKLOAD_NS);
    let central_pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    wait_until(
        "central group artifacts reaped",
        default_timeout(),
        poll_interval(),
        || async {
            let mut left = vgs.list(&ListParams::default()).await?.items.len()
                + vs.list(&ListParams::default()).await?.items.len()
                + grants.list(&ListParams::default()).await?.items.len();
            for uid in &uids {
                let selector = format!("kopiur.home-operations.com/snapshot-uid={uid}");
                left += central_pvcs
                    .list(&ListParams::default().labels(&selector))
                    .await?
                    .items
                    .len();
            }
            Ok((left == 0).then_some(()))
        },
    )
    .await
    .expect("the group, its member snapshots, the grants and the staged PVCs should all go");

    let _ = schedules.delete(schedule, &DeleteParams::default()).await;
    for child in &children {
        let _ = backups
            .delete(&child.name_any(), &DeleteParams::default())
            .await;
    }
    wait_until(
        "grouped Snapshots gone",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(backups
                .list(&by_policy)
                .await?
                .items
                .is_empty()
                .then_some(()))
        },
    )
    .await
    .expect("the grouped Snapshots should be deleted");
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    let _ = crepos
        .delete("e2e-central-repo", &DeleteParams::default())
        .await;
    for src in sources {
        kopiur_e2e::pvc::delete_pvc_and_consumers(&client, WORKLOAD_NS, src)
            .await
            .expect("delete the source PVC");
    }
}
