//! e2e: **stale discovered Snapshots expire on a repository larger than the
//! materialization window** (issue #476).
//!
//! The bug: the bootstrap mover caps the entries it returns (1,000 in
//! production) and flags the listing truncated, and the controller then turned
//! absence expiry OFF — so on any repository past the cap, a discovered row
//! whose snapshot a peer pruned (under its own retention) was kept forever and
//! the row count only grew. The fix ships a membership digest of every listed
//! id alongside the capped window, so "outside the window" and "deleted
//! repository-side" are distinguishable and `status.catalog.coverage` reads
//! `Capped` instead of silently leaking.
//!
//! Reaching 1,000 real snapshots in kind is impractical, so the scenario
//! shrinks the window to 2 with the internal, per-CR
//! `kopiur.home-operations.com/internal-catalog-window` annotation (it can only
//! SHRINK the window — clamped to `1..=MAX_RETURNED_SNAPSHOTS`). Everything
//! else is the production path: a real bootstrap Job, a real kopia listing, a
//! real digest, a real catalog scan.
//!
//! Like `multi_cluster.rs`, every rescan is forced by a spec-change bump (never
//! a `periodicRefresh` timer), so exact-count assertions cannot race a timer.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client, ResourceExt};
use serde::de::DeserializeOwned;

use kopiur_api::{ClusterRepository, Snapshot};
use kopiur_e2e::builders::SeedStep;
use kopiur_e2e::{E2E_NAMESPACE, Need, World, consts, default_timeout, poll_interval, wait_until};

/// The internal window-override annotation (controller:
/// `consts::INTERNAL_CATALOG_WINDOW_ANNOTATION`). Spelled out, not imported: the
/// e2e crate asserts the wire contract, not a Rust constant.
const WINDOW_ANNOTATION: &str = "kopiur.home-operations.com/internal-catalog-window";

fn cr<T: DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("CR JSON deserializes into typed object")
}

async fn status_json(crepos: &Api<ClusterRepository>, name: &str) -> serde_json::Value {
    match crepos.get_opt(name).await.ok().flatten() {
        Some(obj) => serde_json::to_value(&obj)
            .ok()
            .and_then(|v| v.get("status").cloned())
            .unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    }
}

async fn wait_ready(crepos: &Api<ClusterRepository>, name: &str) {
    wait_until(
        &format!("{name} phase=Ready"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(crepos, name).await;
            Ok((s.pointer("/phase").and_then(|v| v.as_str()) == Some("Ready")).then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{name} should bootstrap to Ready: {e}"));
}

async fn run_seeder(client: &Client, name: &str, steps: &[SeedStep<'_>]) {
    kopiur_e2e::apply::run_foreign_seeder(client, E2E_NAMESPACE, name, steps)
        .await
        .expect("foreign kopia seeder");
}

/// Bump `metadata.generation` with an always-different harmless value, forcing
/// a bootstrap-Job recycle + a fresh catalog scan on the next reconcile.
async fn bump_catalog(crepos: &Api<ClusterRepository>, name: &str, max_age_days: i64) {
    crepos
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "spec": { "catalog": { "retain": { "maxAgeDays": max_age_days } } }
            })),
        )
        .await
        .expect("bump catalog.retain.maxAgeDays to force a deterministic rescan");
}

/// Wait until `status.catalog.discoveredBackupCount == want` AND
/// `status.catalog.coverage == coverage` — both are written by the same scan.
async fn wait_catalog(crepos: &Api<ClusterRepository>, name: &str, want: i64, coverage: &str) {
    wait_until(
        &format!("{name} discoveredBackupCount={want} coverage={coverage}"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(crepos, name).await;
            let n = s
                .pointer("/catalog/discoveredBackupCount")
                .and_then(|v| v.as_i64());
            let c = s.pointer("/catalog/coverage").and_then(|v| v.as_str());
            Ok((n == Some(want) && c == Some(coverage)).then_some(()))
        },
    )
    .await
    .unwrap_or_else(|e| {
        panic!("{name} should reach discoveredBackupCount={want} coverage={coverage}: {e}")
    });
}

/// This repository's discovered rows in `ns`, oldest snapshot first.
async fn discovered_rows(client: &Client, ns: &str, repo_uid: &str) -> Vec<Snapshot> {
    let api: Api<Snapshot> = Api::namespaced(client.clone(), ns);
    let selector = format!(
        "kopiur.home-operations.com/origin=discovered,\
         kopiur.home-operations.com/repository-uid={repo_uid}"
    );
    let mut rows = api
        .list(&ListParams::default().labels(&selector))
        .await
        .expect("list discovered Snapshots")
        .items;
    rows.sort_by_key(end_time);
    rows
}

fn end_time(s: &Snapshot) -> String {
    s.status
        .as_ref()
        .and_then(|st| st.timing.as_ref())
        .and_then(|t| t.end_time.clone())
        .unwrap_or_default()
}

fn kopia_id(s: &Snapshot) -> String {
    s.labels()
        .get("kopiur.home-operations.com/snapshot-id")
        .cloned()
        .expect("discovered rows carry the snapshot-id label")
}

/// Write-then-snapshot `n` times under the peer identity, with distinct
/// content per snapshot so each is a genuinely new kopia snapshot.
fn snapshot_steps<'a>(bucket: &'a str, contents: &'a [&'a str]) -> Vec<SeedStep<'a>> {
    let mut steps = vec![SeedStep::ConnectRepo {
        bucket,
        username: "peer",
        hostname: consts::WORKLOAD_NS,
    }];
    for c in contents {
        steps.push(SeedStep::WriteFile {
            dir: "app",
            file: "f.txt",
            content: c,
        });
        steps.push(SeedStep::Snapshot { dir: "app" });
    }
    steps
}

#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn stale_discovered_rows_expire_beyond_the_materialization_window() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio, Need::WorkloadNs])
        .await
        .expect("provision MinIO + workload namespace");
    let client = world.client().clone();
    let crepos: Api<ClusterRepository> = Api::all(client.clone());

    let name = "e2e-catalog-window";
    let bucket = "kopiur-catalog-window";

    // A previous attempt (nextest retries) may have left the CR behind; start
    // clean so a retry is a real retry, not an instant 409. The repository is
    // created fresh in the bucket below, so the bucket is emptied too.
    let _ = crepos.delete(name, &DeleteParams::default()).await;
    wait_until(
        &format!("{name} gone"),
        default_timeout(),
        poll_interval(),
        || async { Ok(crepos.get_opt(name).await?.is_none().then_some(())) },
    )
    .await
    .expect("leftover ClusterRepository should be deleted");
    run_seeder(&client, "e2e-cw-wipe", &[SeedStep::WipeBucket { bucket }]).await;

    // A plain shared repository: no cluster identity, discovered rows land in
    // the namespace named by the snapshot hostname (the #476 topology: the
    // reporter's peer rows were bare-hostname, `foreign=0`).
    crepos
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "ClusterRepository",
                "metadata": { "name": name },
                "spec": {
                    "backend": { "s3": {
                        "bucket": bucket,
                        "endpoint": consts::MINIO_ENDPOINT,
                        "region": "us-east-1",
                        "tls": { "disableTls": true },
                        "auth": { "secretRef": {
                            "name": consts::SECRET_S3_CREDS, "namespace": E2E_NAMESPACE
                        } }
                    }},
                    "encryption": { "passwordSecretRef": {
                        "name": consts::SECRET_S3_CREDS, "namespace": E2E_NAMESPACE,
                        "key": "KOPIA_PASSWORD"
                    } },
                    "create": { "enabled": true },
                    "allowedNamespaces": { "list": [consts::WORKLOAD_NS] },
                    "maintenance": { "enabled": false }
                }
            })),
        )
        .await
        .expect("create ClusterRepository");
    wait_ready(&crepos, name).await;

    // (1) Three peer snapshots, full window → Complete, three rows.
    run_seeder(
        &client,
        "e2e-cw-seed-1",
        &snapshot_steps(bucket, &["one", "two", "three"]),
    )
    .await;
    bump_catalog(&crepos, name, 3650).await;
    wait_catalog(&crepos, name, 3, "Complete").await;

    // (2) Two more, and shrink the window to 2 → Capped. The three older rows
    // fall outside the window but their snapshots still exist: kept.
    run_seeder(
        &client,
        "e2e-cw-seed-2",
        &snapshot_steps(bucket, &["four", "five"]),
    )
    .await;
    crepos
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "metadata": { "annotations": { WINDOW_ANNOTATION: "2" } }
            })),
        )
        .await
        .expect("shrink the materialization window");
    bump_catalog(&crepos, name, 3651).await;
    wait_catalog(&crepos, name, 5, "Capped").await;

    let repo_uid = crepos
        .get(name)
        .await
        .expect("get ClusterRepository")
        .uid()
        .expect("uid");
    let rows = discovered_rows(&client, consts::WORKLOAD_NS, &repo_uid).await;
    assert_eq!(rows.len(), 5, "every snapshot has a row: {rows:#?}");

    // (3) The peer prunes its two OLDEST snapshots — both outside the window.
    // Pre-#476 this is where rows leaked: absent from a truncated listing,
    // never expired, forever.
    let pruned: Vec<String> = rows.iter().take(2).map(kopia_id).collect();
    run_seeder(
        &client,
        "e2e-cw-prune",
        &[
            SeedStep::ConnectRepo {
                bucket,
                username: "peer",
                hostname: consts::WORKLOAD_NS,
            },
            SeedStep::DeleteSnapshots { ids: &pruned },
        ],
    )
    .await;
    bump_catalog(&crepos, name, 3652).await;
    wait_catalog(&crepos, name, 3, "Capped").await;

    let survivors: Vec<String> = discovered_rows(&client, consts::WORKLOAD_NS, &repo_uid)
        .await
        .iter()
        .map(kopia_id)
        .collect();
    assert_eq!(survivors.len(), 3, "exactly the pruned rows expire");
    for id in &pruned {
        assert!(
            !survivors.contains(id),
            "row for pruned snapshot {id} must expire; survivors: {survivors:?}"
        );
    }
    let expected: Vec<String> = rows.iter().skip(2).map(kopia_id).collect();
    assert_eq!(
        survivors, expected,
        "the still-present out-of-window row and both in-window rows are kept"
    );

    let _ = crepos.delete(name, &DeleteParams::default()).await;
}
