//! Leader-election e2e: the chart-default `--leader-elect=true` must produce a
//! real, failover-capable election — not the pre-implementation state where the
//! flag was parsed-and-ignored and `replicaCount > 1` double-reconciled.
//!
//! One sequential scenario (this binary runs with `--test-threads=1` and owns
//! its CI shard, since it scales the operator Deployment):
//! 1. the Lease exists, named after the release, held by the controller pod;
//! 2. at 2 replicas exactly one pod holds the Lease;
//! 3. deleting the holder fails leadership over to the survivor within the
//!    lease window;
//! 4. the NEW leader actually reconciles (a backup runs to `Succeeded`) — the
//!    assertion that would have timed out forever on a standby that never
//!    started its controllers.

#![cfg(all(unix, feature = "e2e"))]

use std::time::Duration;

use kube::Api;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use serde::de::DeserializeOwned;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::Pod;

use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, apply_secret, default_timeout, poll_interval, wait_until,
};

/// The election Lease name: the chart sets `KOPIUR_LEASE_NAME` to the release
/// fullname, and the harness installs release `kopiur`.
const LEASE_NAME: &str = "kopiur";
/// The controller Deployment (`<fullname>-controller`).
const CONTROLLER_DEPLOYMENT: &str = "kopiur-controller";
/// The repository password Secret the chart-installed operator reads.
const CREDS_SECRET: &str = "kopia-creds";

fn cr<T: DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("CR JSON deserializes into typed object")
}

async fn lease_holder(api: &Api<Lease>) -> Option<String> {
    api.get_opt(LEASE_NAME)
        .await
        .ok()
        .flatten()
        .and_then(|l| l.spec)
        .and_then(|s| s.holder_identity)
        .filter(|h| !h.is_empty())
}

async fn scale_controller(client: &kube::Client, replicas: i32) -> anyhow::Result<()> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    api.patch(
        CONTROLLER_DEPLOYMENT,
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "spec": { "replicas": replicas } })),
    )
    .await?;
    wait_until(
        &format!("{CONTROLLER_DEPLOYMENT} ready replicas = {replicas}"),
        default_timeout(),
        poll_interval(),
        || async {
            let d = api.get(CONTROLLER_DEPLOYMENT).await?;
            let ready = d
                .status
                .as_ref()
                .and_then(|s| s.ready_replicas)
                .unwrap_or(0);
            Ok((ready == replicas).then_some(()))
        },
    )
    .await
}

#[tokio::test]
#[ignore = "requires a kind cluster (mise run //crates/e2e:test)"]
async fn leader_election_holds_and_fails_over() -> anyhow::Result<()> {
    let Some(world) = World::connect().await else {
        return Ok(());
    };
    world.ensure(&[Need::Filesystem]).await?;
    let client = world.client();

    let leases: Api<Lease> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // 1. The Lease exists and its holder is the (single) controller pod — the
    //    identity is the pod name, so it must prefix-match the Deployment.
    let holder = wait_until(
        "election Lease has a holder",
        default_timeout(),
        poll_interval(),
        || async { Ok(lease_holder(&leases).await) },
    )
    .await?;
    assert!(
        holder.starts_with(CONTROLLER_DEPLOYMENT),
        "lease holder `{holder}` should be a {CONTROLLER_DEPLOYMENT} pod"
    );
    assert!(
        pods.get_opt(&holder).await?.is_some(),
        "lease holder `{holder}` should be a live pod"
    );

    // 2. Scale to 2 replicas: both come Ready (standbys pass probes while
    //    idle), and the Lease still names exactly one of them.
    scale_controller(client, 2).await?;
    let holder_at_two = lease_holder(&leases).await.expect("lease still held");

    // 3. Kill the holder: the survivor must claim the Lease within the lease
    //    window (15s) plus pod-teardown slack.
    pods.delete(&holder_at_two, &DeleteParams::default())
        .await?;
    let new_holder = wait_until(
        "leadership fails over to the surviving replica",
        Duration::from_secs(90),
        poll_interval(),
        || {
            let leases = leases.clone();
            let old = holder_at_two.clone();
            async move { Ok(lease_holder(&leases).await.filter(|h| *h != old)) }
        },
    )
    .await?;
    assert_ne!(new_holder, holder_at_two, "a new pod must hold the lease");

    // 4. The new leader RECONCILES — the load-bearing assertion. A standby that
    //    never started its controllers would leave this Snapshot Pending forever.
    apply_secret(
        client,
        E2E_NAMESPACE,
        CREDS_SECRET,
        &[("KOPIA_PASSWORD", "e2e-password")],
    )
    .await?;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let repo: Repository = cr(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": "leader-repo", "namespace": E2E_NAMESPACE },
        "spec": {
            "backend": { "filesystem": { "path": "/repo", "volume": { "pvc": { "name": "kopiur-e2e-repo" } } } },
            "encryption": { "passwordSecretRef": { "name": CREDS_SECRET, "key": "KOPIA_PASSWORD" } },
            "create": { "enabled": true }
        }
    }));
    let _ = repos.create(&PostParams::default(), &repo).await;
    wait_status_phase(&repos, "leader-repo", "Ready").await?;

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policy: SnapshotPolicy = cr(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "leader-policy", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "leader-repo" },
            "sources": [ { "pvc": { "name": "e2e-src" } } ],
            "copyMethod": "Direct",
            "retention": { "keepLatest": 3 }
        }
    }));
    let _ = policies.create(&PostParams::default(), &policy).await;

    let snapshots: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let snapshot: Snapshot = cr(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": "leader-snap", "namespace": E2E_NAMESPACE },
        "spec": { "policyRef": { "name": "leader-policy" } }
    }));
    let _ = snapshots.create(&PostParams::default(), &snapshot).await;
    wait_status_phase(&snapshots, "leader-snap", "Succeeded").await?;

    // The new leader's identity must now be recorded on the Lease it renews.
    let final_holder = lease_holder(&leases).await.expect("lease still held");
    assert_eq!(final_holder, new_holder, "failover leader keeps the lease");

    // Scale back down so any later binary in this shard sees the default shape.
    scale_controller(client, 1).await?;
    Ok(())
}

/// Poll a namespaced CR until `status.phase == want`.
async fn wait_status_phase<K>(api: &Api<K>, name: &str, want: &str) -> anyhow::Result<()>
where
    K: kube::Resource + Clone + DeserializeOwned + serde::Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    wait_until(
        &format!("{name} phase={want}"),
        default_timeout(),
        poll_interval(),
        || async {
            match api.get_opt(name).await? {
                Some(obj) => {
                    let v = serde_json::to_value(&obj).unwrap_or_default();
                    let phase = v
                        .get("status")
                        .and_then(|s| s.get("phase"))
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    Ok((phase == want).then_some(()))
                }
                None => Ok(None),
            }
        },
    )
    .await?;
    Ok(())
}
