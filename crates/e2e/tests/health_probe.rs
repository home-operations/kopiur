//! e2e: the opt-in backend health probe (`spec.health.probe`).
//!
//! Proves the headline behavior the feature exists for: after a `Repository` is
//! `Ready`, WIPING its backend out-of-band makes the probe raise a
//! `RepositoryVanished` alert — while the repository **stays `Ready`** (backups
//! are never paused) and is **never auto-recreated** (the pinned `uniqueId` is
//! unchanged and the backend stays empty). This is the data-safety invariant the
//! whole design is built around.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::{DeleteParams, PostParams};

use kopiur_api::Repository;
use kopiur_e2e::builders::{self, SeedStep};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, consts, default_timeout, poll_interval, wait, wait_until,
};

/// A `Repository` on a dedicated MinIO bucket with the health probe enabled at a
/// fast cadence (`interval: 30s`, `failureThreshold: 1`) so the alert fires within
/// the e2e timeout. `create.enabled: true` to prove Part A: even with create on,
/// a once-`Ready` repo is NEVER recreated on a vanish.
fn probe_repository_json(name: &str, bucket: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "backend": { "s3": {
                "bucket": bucket,
                "endpoint": consts::MINIO_ENDPOINT,
                "region": "us-east-1",
                "tls": { "disableTls": true },
                "auth": { "secretRef": { "name": consts::SECRET_S3_CREDS, "namespace": E2E_NAMESPACE } }
            }},
            "encryption": {
                "passwordSecretRef": { "name": consts::SECRET_S3_CREDS, "key": "KOPIA_PASSWORD" }
            },
            "create": { "enabled": true },
            // The managed Maintenance is irrelevant here; keep the test focused.
            "maintenance": { "enabled": false },
            "health": {
                "probe": { "enabled": true, "interval": "30s", "failureThreshold": 1 }
            }
        }
    })
}

fn status_value(repo: &Repository) -> serde_json::Value {
    serde_json::to_value(repo)
        .ok()
        .and_then(|v| v.get("status").cloned())
        .unwrap_or_default()
}

fn condition(status: &serde_json::Value, type_: &str, field: &str) -> Option<String> {
    status
        .get("conditions")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some(type_))
        .and_then(|c| c.get(field).and_then(|s| s.as_str()))
        .map(str::to_string)
}

#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn probe_alerts_on_wipe_but_stays_ready_and_never_recreates() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio])
        .await
        .expect("provision MinIO + buckets");
    let client = world.client().clone();
    let bucket = "kopiur-health-probe";
    let repo = "e2e-health-probe";

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    repos
        .create(
            &PostParams::default(),
            &serde_json::from_value(probe_repository_json(repo, bucket))
                .expect("Repository JSON deserializes"),
        )
        .await
        .expect("create Repository");

    // 1. First bootstrap creates the repo and pins a uniqueId.
    wait_until(
        &format!("{repo} Ready"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_value(&repos.get(repo).await?);
            Ok((s.get("phase").and_then(|p| p.as_str()) == Some("Ready")).then_some(()))
        },
    )
    .await
    .expect("repository becomes Ready");
    let original_unique_id = status_value(&repos.get(repo).await.unwrap())
        .get("uniqueId")
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .expect("Ready repository pins a uniqueId");

    // 2. Wipe the backend out-of-band: delete every object in the bucket so the
    //    kopia format blob is gone (the backend itself stays reachable).
    let wipe = builders::foreign_kopia_pod(
        E2E_NAMESPACE,
        "e2e-health-probe-wipe",
        &[SeedStep::WipeBucket { bucket }],
    );
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    pods.create(&PostParams::default(), &wipe)
        .await
        .expect("create wipe pod");
    wait::pod_succeeded(&client, E2E_NAMESPACE, "e2e-health-probe-wipe")
        .await
        .expect("wipe pod empties the bucket");

    // 3. The probe must raise RepositoryVanished (backend reachable, repo absent).
    wait_until(
        &format!("{repo} BackendReachable=False/RepositoryVanished"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_value(&repos.get(repo).await?);
            let vanished = condition(&s, "BackendReachable", "status").as_deref() == Some("False")
                && condition(&s, "BackendReachable", "reason").as_deref()
                    == Some("RepositoryVanished");
            Ok(vanished.then_some(()))
        },
    )
    .await
    .expect("probe raises RepositoryVanished after the wipe");

    // 4. The load-bearing assertions: the repository stayed Ready (backups not
    //    paused) and was NEVER recreated (same uniqueId; the backend is still empty
    //    because Part A forbids auto-create over a once-Ready repo).
    let final_status = status_value(&repos.get(repo).await.unwrap());
    assert_eq!(
        final_status.get("phase").and_then(|p| p.as_str()),
        Some("Ready"),
        "phase must stay Ready (alert-only) — a vanish must not halt backups"
    );
    assert_eq!(
        final_status.get("uniqueId").and_then(|u| u.as_str()),
        Some(original_unique_id.as_str()),
        "uniqueId must be unchanged — kopiur must NEVER recreate a once-Ready repo"
    );

    // Cleanup (best-effort).
    let _ = repos.delete(repo, &DeleteParams::default()).await;
    let _ = pods
        .delete("e2e-health-probe-wipe", &DeleteParams::default())
        .await;
}
