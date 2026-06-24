//! e2e: the opt-in backup preflight (`SnapshotPolicy.spec.preflight`).
//!
//! Proves the two headline behaviors against a Helm-deployed operator in kind:
//!
//! 1. A **satisfiable** preflight lets the backup run to `Succeeded`.
//! 2. An **unsatisfiable** preflight holds the `Snapshot` in `Pending`
//!    (`PreflightFailed`) with **no mover Job**, then transitions it to `Failed`
//!    after the bounded `timeout` — and a preflight-`Failed` Snapshot (which never
//!    produced a kopia snapshot) deletes cleanly via its finalizer (no hung Job).
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

use kube::Api;
use kube::api::{DeleteParams, PostParams};
use serde::de::DeserializeOwned;

use k8s_openapi::api::batch::v1::Job;
use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

const CREDS_SECRET: &str = "kopia-creds";

fn cr<T: DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("CR JSON deserializes into typed object")
}

fn repository_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "backend": { "filesystem": { "path": "/repo", "volume": { "pvc": { "name": "kopiur-e2e-repo" } } } },
            "encryption": { "passwordSecretRef": { "name": CREDS_SECRET, "key": "KOPIA_PASSWORD" } },
            "create": { "enabled": true }
        }
    })
}

/// A `SnapshotPolicy` with a `preflight` block: one check plus a `timeout`.
fn policy_with_preflight(
    name: &str,
    repo: &str,
    src_pvc: &str,
    check_name: &str,
    expr: &str,
    timeout: &str,
) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": repo },
            "sources": [ { "pvc": { "name": src_pvc } } ],
            "retention": { "keepLatest": 5 },
            "preflight": {
                "timeout": timeout,
                "checks": [ { "name": check_name, "expr": expr } ]
            }
        }
    })
}

fn snapshot_json(name: &str, policy: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": { "policyRef": { "name": policy }, "deletionPolicy": "Retain" }
    })
}

async fn snapshot_status(api: &Api<Snapshot>, name: &str) -> serde_json::Value {
    match api.get_opt(name).await.ok().flatten() {
        Some(obj) => serde_json::to_value(&obj)
            .ok()
            .and_then(|v| v.get("status").cloned())
            .unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    }
}

fn phase_of(status: &serde_json::Value) -> Option<String> {
    status
        .get("phase")
        .and_then(|p| p.as_str())
        .map(str::to_string)
}

/// The `Ready` condition's `reason` (Kopiur surfaces `PreflightFailed` there).
fn ready_reason(status: &serde_json::Value) -> Option<String> {
    status
        .get("conditions")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("Ready"))
        .and_then(|c| c.get("reason").and_then(|r| r.as_str()))
        .map(str::to_string)
}

async fn wait_snapshot_phase(api: &Api<Snapshot>, name: &str, want: &str) {
    wait_until(
        &format!("{name} phase={want}"),
        default_timeout(),
        poll_interval(),
        || async {
            Ok(
                (phase_of(&snapshot_status(api, name).await).as_deref() == Some(want))
                    .then_some(()),
            )
        },
    )
    .await
    .unwrap_or_else(|e| panic!("Snapshot {name} should reach {want}: {e}"));
}

#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn preflight_satisfiable_runs_the_backup() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    repos
        .create(&PostParams::default(), &cr(repository_json("e2e-pf-repo")))
        .await
        .expect("create Repository");

    // A check that is true the moment the repository is Ready ⇒ the backup runs.
    policies
        .create(
            &PostParams::default(),
            &cr(policy_with_preflight(
                "e2e-pf-ok",
                "e2e-pf-repo",
                "e2e-src",
                "repo-ready",
                "repository.ready",
                "10m",
            )),
        )
        .await
        .expect("create SnapshotPolicy");
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json("e2e-pf-ok-backup", "e2e-pf-ok")),
        )
        .await
        .expect("create Snapshot");

    wait_snapshot_phase(&backups, "e2e-pf-ok-backup", "Succeeded").await;

    // Cleanup (best-effort).
    let _ = backups
        .delete("e2e-pf-ok-backup", &DeleteParams::default())
        .await;
    let _ = policies.delete("e2e-pf-ok", &DeleteParams::default()).await;
}

#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn preflight_unsatisfiable_blocks_then_fails_and_deletes_cleanly() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    repos
        .create(
            &PostParams::default(),
            &cr(repository_json("e2e-pf-repo-x")),
        )
        .await
        .expect("create Repository");

    // An impossible check with a short timeout: hold Pending, then fail fast.
    policies
        .create(
            &PostParams::default(),
            &cr(policy_with_preflight(
                "e2e-pf-block",
                "e2e-pf-repo-x",
                "e2e-src",
                "impossible",
                "repository.snapshotCount > 1000000",
                "30s",
            )),
        )
        .await
        .expect("create SnapshotPolicy");
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json("e2e-pf-block-backup", "e2e-pf-block")),
        )
        .await
        .expect("create Snapshot");

    // 1. Held in Pending with reason PreflightFailed — and NO mover Job is created.
    wait_until(
        "e2e-pf-block-backup Pending/PreflightFailed",
        default_timeout(),
        poll_interval(),
        || async {
            let s = snapshot_status(&backups, "e2e-pf-block-backup").await;
            let blocked = phase_of(&s).as_deref() == Some("Pending")
                && ready_reason(&s).as_deref() == Some("PreflightFailed");
            Ok(blocked.then_some(()))
        },
    )
    .await
    .expect("Snapshot must be held Pending with PreflightFailed");
    assert!(
        jobs.get_opt("e2e-pf-block-backup")
            .await
            .expect("list Jobs")
            .is_none(),
        "no mover Job may be created while preflight blocks the backup"
    );

    // 2. After the 30s timeout the Snapshot transitions to Failed (bounded).
    wait_snapshot_phase(&backups, "e2e-pf-block-backup", "Failed").await;

    // 3. A preflight-Failed Snapshot never produced a kopia snapshot, so deleting it
    //    removes the finalizer cleanly (no delete Job, no hang).
    backups
        .delete("e2e-pf-block-backup", &DeleteParams::default())
        .await
        .expect("delete preflight-Failed Snapshot");
    wait_until(
        "e2e-pf-block-backup deleted",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(backups
                .get_opt("e2e-pf-block-backup")
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("preflight-Failed Snapshot should delete cleanly");

    // Cleanup (best-effort).
    let _ = policies
        .delete("e2e-pf-block", &DeleteParams::default())
        .await;
}
