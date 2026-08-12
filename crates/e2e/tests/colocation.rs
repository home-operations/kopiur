//! e2e: RWO source-PVC node co-location (the Multi-Attach fix).
//!
//! A `ReadWriteOnce` PVC can only be *attached* to one node at a time. When an app
//! pod already holds an RWO PVC on node N and the backup mover lands elsewhere, the
//! mover pod is stuck `Multi-Attach error`. The controller resolves the node the PVC
//! is attached to (via the consuming pod) and pins the mover there with a required
//! `kubernetes.io/hostname` nodeAffinity, so it co-locates with the workload and the
//! kubelet can mount the volume.
//!
//! These scenarios reproduce the exact reported setup — an RWO PVC held by a running
//! pod — and assert the mover Job is tied to that pod's node (default `Auto` mode),
//! that the snapshot then succeeds, and that `Disabled` mode opts out of the pin.
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
use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

/// The well-known node-hostname label the mover is pinned to.
const HOSTNAME_LABEL: &str = "kubernetes.io/hostname";

/// Extract the `kubernetes.io/hostname` `In` values from a mover Job's REQUIRED
/// nodeAffinity (`None` if the mover carries no such pin).
fn hostname_pin(job: &Job) -> Option<Vec<String>> {
    job.spec
        .as_ref()?
        .template
        .spec
        .as_ref()?
        .affinity
        .as_ref()?
        .node_affinity
        .as_ref()?
        .required_during_scheduling_ignored_during_execution
        .as_ref()?
        .node_selector_terms
        .iter()
        .flat_map(|t| t.match_expressions.iter().flatten())
        .find(|e| e.key == HOSTNAME_LABEL && e.operator == "In")
        .and_then(|e| e.values.clone())
}

/// Create an RWO PVC and a long-running consumer pod that mounts it, then wait until
/// the pod is `Running` on a node (which binds + attaches the RWO volume). Returns the
/// node the PVC is now attached to.
async fn rwo_pvc_held_on_a_node(client: &kube::Client, pvc: &str, consumer: &str) -> String {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    pvcs.create(
        &PostParams::default(),
        &kopiur_e2e::builders::dynamic_pvc(E2E_NAMESPACE, pvc, "100Mi"),
    )
    .await
    .unwrap_or_else(|e| panic!("create RWO PVC {pvc}: {e}"));

    pods.create(
        &PostParams::default(),
        &kopiur_e2e::builders::sleeper_pod(
            E2E_NAMESPACE,
            consumer,
            &[("app", consumer)],
            pvc,
            "/data",
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("create consumer pod {consumer}: {e}"));

    // The dynamic (WaitForFirstConsumer) PVC binds and attaches to whichever node the
    // consumer lands on; capture that node once the pod is Running.
    wait_until(
        &format!("consumer {consumer} Running on a node"),
        default_timeout(),
        poll_interval(),
        || async {
            let Some(p) = pods.get_opt(consumer).await? else {
                return Ok(None);
            };
            let running = p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running");
            let node = p.spec.as_ref().and_then(|s| s.node_name.clone());
            Ok(running.then_some(node).flatten())
        },
    )
    .await
    .unwrap_or_else(|_| panic!("consumer {consumer} should reach Running on a node"))
}

/// THE HEADLINE. With the default `Auto` mode, a backup whose source is an RWO PVC
/// held by a running pod gets its mover Job PINNED (required nodeAffinity on
/// `kubernetes.io/hostname`) to exactly that pod's node — so on a real multi-node
/// cluster it co-locates with the workload instead of failing `Multi-Attach error` —
/// and the snapshot then succeeds while the volume is concurrently mounted.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn auto_pins_rwo_source_mover_to_the_consumer_node() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_repo(&client, "colocation").await;

    let pvc = "e2e-colo-src";
    let consumer = "e2e-colo-consumer";
    let node = rwo_pvc_held_on_a_node(&client, pvc, consumer).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-colo-repo";
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(repo, "colocation", serde_json::json!({}))),
        )
        .await
        .expect("create Repository");
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                "e2e-colo-policy",
                "Repository",
                repo,
                // Snapshot the held RWO PVC (overrides the shared `e2e-src` source).
                serde_json::json!({ "sources": [ { "pvc": { "name": pvc } } ] }),
            )),
        )
        .await
        .expect("create SnapshotPolicy over the RWO source");
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-colo-backup",
                "e2e-colo-policy",
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Snapshot");

    // The mover Job must be pinned to the node holding the RWO PVC.
    let bjob = wait_for_job(&jobs, "e2e-colo-backup").await;
    assert_eq!(
        hostname_pin(&bjob).as_deref(),
        Some([node.clone()].as_slice()),
        "Auto mode must pin the RWO-source mover to the consumer pod's node ({node})"
    );

    // And the snapshot succeeds while the consumer still holds the volume (proving the
    // co-mount on the same node works end-to-end).
    wait_phase(&backups, "e2e-colo-backup", "Succeeded")
        .await
        .expect("snapshot of the co-located RWO PVC should Succeed");

    // Cleanup.
    let _ = backups
        .delete("e2e-colo-backup", &DeleteParams::default())
        .await;
    let _ = policies
        .delete("e2e-colo-policy", &DeleteParams::default())
        .await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = pods.delete(consumer, &DeleteParams::default()).await;
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = pvcs.delete(pvc, &DeleteParams::default()).await;
}

/// Read the `SourcePvcAvailable` condition off a Snapshot's status, as
/// `(status, reason, lastTransitionTime)`.
fn source_pvc_condition(status: &serde_json::Value) -> Option<(String, String, String)> {
    status
        .get("conditions")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("SourcePvcAvailable"))
        .map(|c| {
            (
                c["status"].as_str().unwrap_or_default().to_string(),
                c["reason"].as_str().unwrap_or_default().to_string(),
                c["lastTransitionTime"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
}

/// Missing-source-PVC bounded outcome (#382 M5). A Snapshot whose DIRECT source
/// PVC does not exist must NOT retry-hot-loop forever: it parks at
/// `phase: Pending` behind the registered `SourcePvcAvailable=False` /
/// `SourcePvcMissing` structural gate with byte-stable status (no churn — the
/// object's resourceVersion and the gate's lastTransitionTime hold still), a
/// Warning Event fires once on the False transition, and recreating the PVC
/// BEFORE the deadline recovers the backup to `Succeeded` with the gate flipped
/// `True`. (The deadline→Failed leg is pinned by hermetic pure-fn unit tests —
/// shortening the suite-wide deadline via Helm would destabilize the other
/// colocation/retention scenarios.)
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn missing_source_pvc_parks_with_a_gate_and_recovers_on_recreate() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_repo(&client, "colocation-missing").await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let events: Api<Event> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-colo-miss-repo";
    let pvc = "e2e-colo-miss-src";
    let policy = "e2e-colo-miss-policy";
    let backup = "e2e-colo-miss-backup";

    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                repo,
                "colocation-missing",
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Repository");
    // The recipe names a PVC that does NOT exist (the "deleted source" shape).
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                policy,
                "Repository",
                repo,
                serde_json::json!({ "sources": [ { "pvc": { "name": pvc } } ] }),
            )),
        )
        .await
        .expect("create SnapshotPolicy over the missing source PVC");
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                backup,
                policy,
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Snapshot");

    // 1) The park: SourcePvcAvailable=False / SourcePvcMissing, phase Pending.
    let (_, _, first_transition) = wait_until(
        "SourcePvcAvailable=False (SourcePvcMissing) on the Snapshot",
        default_timeout(),
        poll_interval(),
        || async {
            let status = status_json(&backups, backup).await;
            let parked = source_pvc_condition(&status)
                .filter(|(s, r, _)| s == "False" && r == "SourcePvcMissing");
            let pending = status.get("phase").and_then(|p| p.as_str()) == Some("Pending");
            Ok(parked.filter(|_| pending))
        },
    )
    .await
    .expect("the Snapshot must park behind the SourcePvcAvailable gate at phase Pending");

    // 2) The Warning Event fired (transition-gated) and names the PVC.
    let ev = wait_until(
        "a SourcePvcMissing Warning Event regarding the Snapshot",
        default_timeout(),
        poll_interval(),
        || async {
            let list = events.list(&ListParams::default()).await?;
            Ok(list.items.into_iter().find(|e| {
                e.type_.as_deref() == Some("Warning")
                    && e.reason.as_deref() == Some("SourcePvcMissing")
                    && e.regarding.as_ref().is_some_and(|r| {
                        r.kind.as_deref() == Some("Snapshot") && r.name.as_deref() == Some(backup)
                    })
            }))
        },
    )
    .await
    .expect("the False transition must publish a Warning Event");
    let note = ev.note.unwrap_or_default();
    assert!(
        note.contains(&format!("{E2E_NAMESPACE}/{pvc}")),
        "the Event note must name the missing PVC, got: {note}"
    );

    // 3) No hot loop: the parked status is byte-stable — over an observation
    //    window longer than the old 30s retry cadence, neither the object's
    //    resourceVersion nor the gate's lastTransitionTime may move, and the
    //    phase stays Pending (not Failed: the default deadline is 30 min).
    let before = backups.get(backup).await.expect("read parked Snapshot");
    let rv_before = before.metadata.resource_version.clone().expect("rv");
    tokio::time::sleep(std::time::Duration::from_secs(35)).await;
    let after = backups.get(backup).await.expect("re-read parked Snapshot");
    assert_eq!(
        after.metadata.resource_version.as_deref(),
        Some(rv_before.as_str()),
        "a parked Snapshot must not churn its own status (the hot-loop regression)"
    );
    let status = status_json(&backups, backup).await;
    let (s, _, transition) = source_pvc_condition(&status).expect("the gate condition persists");
    assert_eq!(s, "False");
    assert_eq!(
        transition, first_transition,
        "the deadline anchor (lastTransitionTime) must be stamped once, never re-stamped"
    );
    assert_eq!(
        status.get("phase").and_then(|p| p.as_str()),
        Some("Pending")
    );

    // 4) Recovery: recreate the PVC pre-deadline; the next reconcile launches
    //    the mover (the WaitForFirstConsumer claim binds to it) and the gate
    //    flips True. Nudge an immediate reconcile via a metadata touch — no PVC
    //    watch exists by design, so recovery is otherwise slot/requeue-cadenced.
    pvcs.create(
        &PostParams::default(),
        &kopiur_e2e::builders::dynamic_pvc(E2E_NAMESPACE, pvc, "100Mi"),
    )
    .await
    .expect("recreate the source PVC");
    backups
        .patch_metadata(
            backup,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "metadata": { "annotations": { "kopiur.home-operations.com/e2e-nudge": "pvc-recreated" } }
            })),
        )
        .await
        .expect("nudge the Snapshot reconcile");

    wait_phase(&backups, backup, "Succeeded")
        .await
        .expect("the backup must recover once the source PVC exists again");
    let status = status_json(&backups, backup).await;
    let (s, r, _) = source_pvc_condition(&status)
        .expect("the gate condition is cleared in place, never dropped");
    assert_eq!(
        (s.as_str(), r.as_str()),
        ("True", "SourcePvcFound"),
        "recovery must flip the existing gate condition True"
    );

    // Cleanup.
    let _ = backups.delete(backup, &DeleteParams::default()).await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
    let _ = pvcs.delete(pvc, &DeleteParams::default()).await;
}

/// The escape hatch: `moverDefaults.sourceColocation.mode: Disabled` must leave the
/// mover UNPINNED (no injected `kubernetes.io/hostname` nodeAffinity) even for an RWO
/// source held by a running pod — the pre-fix scheduling behavior, for topologies that
/// manage placement themselves. The snapshot still succeeds (single-node cluster).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn disabled_mode_leaves_the_rwo_mover_unpinned() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_repo(&client, "colocation-off").await;

    let pvc = "e2e-colo-off-src";
    let consumer = "e2e-colo-off-consumer";
    let _node = rwo_pvc_held_on_a_node(&client, pvc, consumer).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-colo-off-repo";
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                repo,
                "colocation-off",
                serde_json::json!({
                    "moverDefaults": { "sourceColocation": { "mode": "Disabled" } }
                }),
            )),
        )
        .await
        .expect("create Repository with sourceColocation.mode=Disabled");
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                "e2e-colo-off-policy",
                "Repository",
                repo,
                serde_json::json!({ "sources": [ { "pvc": { "name": pvc } } ] }),
            )),
        )
        .await
        .expect("create SnapshotPolicy");
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-colo-off-backup",
                "e2e-colo-off-policy",
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Snapshot");

    let bjob = wait_for_job(&jobs, "e2e-colo-off-backup").await;
    assert!(
        hostname_pin(&bjob).is_none(),
        "Disabled mode must not inject a hostname nodeAffinity pin, got {:?}",
        hostname_pin(&bjob)
    );
    wait_phase(&backups, "e2e-colo-off-backup", "Succeeded")
        .await
        .expect("snapshot should still Succeed with co-location disabled");

    // Cleanup.
    let _ = backups
        .delete("e2e-colo-off-backup", &DeleteParams::default())
        .await;
    let _ = policies
        .delete("e2e-colo-off-policy", &DeleteParams::default())
        .await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = pods.delete(consumer, &DeleteParams::default()).await;
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = pvcs.delete(pvc, &DeleteParams::default()).await;
}
