//! Namespaced-install e2e (`installScope: namespaced`, the chart default): a
//! Role-only install must ACTUALLY reconcile its own namespace — the regression
//! this guards is the pre-scoping controller whose cluster-wide watches 403'd
//! against Role RBAC forever, leaving a Ready-looking operator that reconciled
//! nothing — and must ignore everything outside it.
//!
//! Scope detection is SELF-SERVED, not env-wired: the test reads the deployed
//! controller Deployment's args and runs iff they carry `--namespace=…` (the
//! chart stamps exactly one of `--namespace`/`--cluster-scope`). An env-based
//! gate could silently skip forever if the CI plumbing was renamed — the shard
//! that exists solely for this test would then go green while testing nothing.

#![cfg(all(unix, feature = "e2e"))]

use std::time::Duration;

use kube::Api;
use kube::api::PostParams;
use serde::de::DeserializeOwned;

use k8s_openapi::api::apps::v1::Deployment;

use kopiur_api::{ClusterRepository, Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, apply_secret, default_timeout, ensure_namespace, poll_interval,
    wait_until,
};

/// The repository password Secret the chart-installed operator reads.
const CREDS_SECRET: &str = "kopia-creds";
/// A namespace the (namespaced) operator must NOT touch.
const OUTSIDE_NS: &str = "kopiur-e2e-outside";

fn cr<T: DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("CR JSON deserializes into typed object")
}

fn repository_json(name: &str, ns: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": name, "namespace": ns },
        "spec": {
            "backend": { "filesystem": { "path": "/repo", "volume": { "pvc": { "name": "kopiur-e2e-repo" } } } },
            "encryption": { "passwordSecretRef": { "name": CREDS_SECRET, "key": "KOPIA_PASSWORD" } },
            "create": { "enabled": true }
        }
    })
}

/// Whether the deployed operator is a namespaced install, read from the
/// controller Deployment's chart-stamped argv (`--namespace=…` vs
/// `--cluster-scope`). Ground truth from the cluster, so a broken CI env
/// wiring can never silently skip this scenario against a namespaced install.
async fn deployed_scope_is_namespaced(client: &kube::Client) -> anyhow::Result<bool> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let deploy = api.get("kopiur-controller").await?;
    let args = deploy
        .spec
        .and_then(|s| s.template.spec)
        .and_then(|p| p.containers.into_iter().next())
        .and_then(|c| c.args)
        .unwrap_or_default();
    if args.iter().any(|a| a.starts_with("--namespace=")) {
        return Ok(true);
    }
    if args.iter().any(|a| a == "--cluster-scope") {
        return Ok(false);
    }
    anyhow::bail!(
        "controller Deployment args carry neither --namespace nor --cluster-scope \
         (args: {args:?}); the chart contract changed and this test needs updating"
    );
}

#[tokio::test]
#[ignore = "requires a kind cluster installed with installScope=namespaced"]
async fn namespaced_install_reconciles_its_namespace_and_only_it() -> anyhow::Result<()> {
    let Some(world) = World::connect().await else {
        return Ok(());
    };
    let client = world.client();
    if !deployed_scope_is_namespaced(client).await? {
        eprintln!(
            "skipping: the deployed operator is cluster-scoped (this scenario needs a chart \
             installed with installScope=namespaced — the install-scope CI shard sets \
             KOPIUR_E2E_HELM_SET=installScope=namespaced)"
        );
        return Ok(());
    }
    world.ensure(&[Need::Filesystem]).await?;

    // --- positive half: a full backup lifecycle INSIDE the release namespace.
    // This is the load-bearing regression guard: the pre-scoping controller
    // never received a single watch event under Role RBAC, so this Snapshot
    // would sit with no status forever.
    apply_secret(
        client,
        E2E_NAMESPACE,
        CREDS_SECRET,
        &[("KOPIA_PASSWORD", "e2e-password")],
    )
    .await?;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let repo: Repository = cr(repository_json("scoped-repo", E2E_NAMESPACE));
    let _ = repos.create(&PostParams::default(), &repo).await;
    wait_status_phase(&repos, "scoped-repo", "Ready").await?;

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policy: SnapshotPolicy = cr(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": "scoped-policy", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "scoped-repo" },
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
        "metadata": { "name": "scoped-snap", "namespace": E2E_NAMESPACE },
        "spec": { "policyRef": { "name": "scoped-policy" } }
    }));
    let _ = snapshots.create(&PostParams::default(), &snapshot).await;
    wait_status_phase(&snapshots, "scoped-snap", "Succeeded").await?;

    // --- negative half: CRs OUTSIDE the release namespace stay untouched (the
    // operator has no RBAC there and — after the scoping fix — no watch either).
    ensure_namespace(client, OUTSIDE_NS).await?;
    apply_secret(
        client,
        OUTSIDE_NS,
        CREDS_SECRET,
        &[("KOPIA_PASSWORD", "e2e-password")],
    )
    .await?;
    let outside_repos: Api<Repository> = Api::namespaced(client.clone(), OUTSIDE_NS);
    let outside: Repository = cr(repository_json("outside-repo", OUTSIDE_NS));
    let _ = outside_repos.create(&PostParams::default(), &outside).await;

    // A ClusterRepository is likewise out of reach for a namespaced install.
    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let crepo: ClusterRepository = cr(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "ClusterRepository",
        "metadata": { "name": "scoped-out-crepo" },
        "spec": {
            "backend": { "filesystem": { "path": "/repo", "volume": { "pvc": { "name": "kopiur-e2e-repo" } } } },
            "encryption": { "passwordSecretRef": {
                "name": CREDS_SECRET, "namespace": E2E_NAMESPACE, "key": "KOPIA_PASSWORD"
            } },
            "create": { "enabled": true },
            "allowedNamespaces": { "all": true }
        }
    }));
    let _ = crepos.create(&PostParams::default(), &crepo).await;

    // Grace window: long enough that a (buggy) cluster-wide watch would have
    // reconciled and stamped a status; both objects must still have none.
    tokio::time::sleep(Duration::from_secs(30)).await;
    let outside_status = outside_repos
        .get("outside-repo")
        .await?
        .status
        .and_then(|s| serde_json::to_value(s).ok())
        .filter(|v| !v.is_null());
    assert!(
        outside_status.is_none(),
        "a namespaced install must not reconcile a foreign namespace, found status: \
         {outside_status:?}"
    );
    let crepo_status = crepos
        .get("scoped-out-crepo")
        .await?
        .status
        .and_then(|s| serde_json::to_value(s).ok())
        .filter(|v| !v.is_null());
    assert!(
        crepo_status.is_none(),
        "a namespaced install must not reconcile ClusterRepository, found status: \
         {crepo_status:?}"
    );
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
