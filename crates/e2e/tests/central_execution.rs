//! e2e: `ClusterRepository.spec.execution.mode: Central`: the backup mover runs
//! in the OPERATOR namespace and reads a cross-namespace CSI snapshot restore.
//!
//! Two scenarios:
//!
//! 1. **Central backup**: a CSI PVC in a workload namespace that holds neither the
//!    repository credentials nor the repository volume is backed up by a mover Job
//!    in the operator namespace. Nothing runs in, and no Secret enters, the
//!    workload namespace; every transient object is reaped afterwards.
//! 2. **Refusal**: a policy the central path cannot serve (`copyMethod: Direct`)
//!    fails with the reason, instead of quietly running a mover beside the source.
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

use kube::api::{DeleteParams, ListParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Api, Client, ResourceExt};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
use k8s_openapi::api::rbac::v1::RoleBinding;
use kopiur_api::{ClusterRepository, Snapshot, SnapshotPolicy};
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

    let _ = policies.delete(policy, &DeleteParams::default()).await;
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

    // A tenant-chosen mount path is refused too. `sourcePathOverride` is where
    // the source volume is mounted in the mover pod, and a central mover pod
    // holds the repository credentials: a volume over `/usr/local/bin` would
    // replace the mover binary with whatever the tenant put on the volume.
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
    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            WORKLOAD_NS,
            override_policy,
            "ClusterRepository",
            "e2e-central-refuse-repo",
            serde_json::json!({
                "copyMethod": "Snapshot",
                "sources": [ {
                    "pvc": { "name": "does-not-matter" },
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
    let condition = wait_condition(&backups, override_backup, "SourceStaged", "False")
        .await
        .expect("the Snapshot should be refused");
    assert_eq!(
        condition["reason"].as_str(),
        Some("CentralExecutionUnsupported"),
        "{condition}"
    );
    let message = condition["message"].as_str().unwrap_or("");
    assert!(message.contains("sourcePathOverride"), "{message}");
    let override_uid = backups
        .get(override_backup)
        .await
        .expect("get Snapshot")
        .uid()
        .unwrap();
    let central_jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let launched: Vec<String> = central_jobs
        .list(&ListParams::default().labels(&format!(
            "kopiur.home-operations.com/snapshot-uid={override_uid}"
        )))
        .await
        .expect("list central jobs")
        .items
        .iter()
        .map(ResourceExt::name_any)
        .collect();
    assert!(
        launched.is_empty(),
        "a refused Snapshot must not launch a central mover, found {launched:?}"
    );

    let _ = backups
        .delete(override_backup, &DeleteParams::default())
        .await;
    let _ = policies
        .delete(override_policy, &DeleteParams::default())
        .await;
    let _ = backups.delete(backup, &DeleteParams::default()).await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    let _ = crepos
        .delete("e2e-central-refuse-repo", &DeleteParams::default())
        .await;
}
