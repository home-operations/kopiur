//! e2e (#613): the mover's kopia cache budgets follow the cache volume, and a
//! `Persistent` cache PVC grows when `capacity` is raised.
//!
//! 1. `{ percent }` budgets resolve against `capacity`, an unset budget defaults to
//!    its share, the result reaches kopia (the work-spec `cache` block the mover
//!    turns into `--content/metadata-cache-size-mb`) and is pinned on the Snapshot;
//!    budgets that overflow the volume are refused at admission.
//! 2. Raising `capacity` grows the existing persistent cache PVC (grow-only); a
//!    smaller `capacity` is refused with a Warning Event, never applied.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster. Runs in the
//! `reshape-core` shard, which installs the CSI stack (`csi-hostpath-sc`, which
//! allows volume expansion).

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use k8s_openapi::api::events::v1::Event;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use kube::{Api, Client};

use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

/// `status.resolved.cache` of a Snapshot, as JSON.
async fn resolved_cache(backups: &Api<Snapshot>, name: &str) -> serde_json::Value {
    status_json(backups, name)
        .await
        .pointer("/resolved/cache")
        .cloned()
        .unwrap_or_default()
}

/// Create a filesystem Repository over the isolated `subpath` and wait for Ready.
async fn ready_repo(client: &Client, repo: &str, subpath: &str) -> Api<Repository> {
    ensure_repo(client, subpath).await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repos,
        &cr(repository_json(repo, subpath, serde_json::json!({}))),
        "create Repository",
    )
    .await;
    wait_phase(&repos, repo, "Ready")
        .await
        .expect("Repository should bootstrap to Ready");
    repos
}

/// #613: `{ percent }` budgets are resolved against the cache `capacity` and reach
/// kopia; an unset budget defaults to its share; the values are pinned on the
/// Snapshot. Before #613 kopia got either nothing (its multi-GiB defaults) or
/// absolute MiB that could exceed the volume.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn percent_cache_budgets_reach_kopia_and_are_pinned() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    let (repo, policy, backup) = ("e2e-cb-repo", "e2e-cb-policy", "e2e-cb-backup");
    let repos = ready_repo(&client, repo, "cachebudget").await;
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // 1Gi Ephemeral cache, content 50%, metadata left to its 15% default.
    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            E2E_NAMESPACE,
            policy,
            "Repository",
            repo,
            serde_json::json!({
                "mover": { "cache": {
                    "capacity": "1Gi",
                    "mode": "Ephemeral",
                    "contentCacheSize": { "percent": 50 }
                } }
            }),
        )),
        "create SnapshotPolicy with a percent cache budget",
    )
    .await;
    let landed = policies.get(policy).await.expect("read the policy back");
    assert!(
        landed
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.cache.as_ref())
            .and_then(|c| c.content_cache_size.as_ref())
            .is_some(),
        "contentCacheSize must survive the apiserver (a stale CRD would prune it)"
    );
    create_idempotent(
        &backups,
        &cr(snapshot_json(
            E2E_NAMESPACE,
            backup,
            policy,
            serde_json::json!({}),
        )),
        "create Snapshot",
    )
    .await;

    let ws = wait_for_work_spec_json(&jobs, backup).await;
    assert_eq!(
        ws.pointer("/cache/contentCacheSizeMb"),
        Some(&serde_json::json!(512)),
        "50% of 1Gi must reach kopia as --content-cache-size-mb 512: {ws}"
    );
    assert_eq!(
        ws.pointer("/cache/metadataCacheSizeMb"),
        Some(&serde_json::json!(153)),
        "the unset metadata budget must default to 15% of 1Gi: {ws}"
    );

    wait_phase(&backups, backup, "Succeeded")
        .await
        .expect("a backup with percent cache budgets must succeed");
    let pinned = resolved_cache(&backups, backup).await;
    assert_eq!(pinned["capacity"], "1Gi", "{pinned}");
    assert_eq!(pinned["contentCacheSizeMb"], 512, "{pinned}");
    assert_eq!(pinned["metadataCacheSizeMb"], 153, "{pinned}");

    // Admission refuses budgets that overflow the volume — the #613 shape.
    let err = policies
        .patch(
            policy,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "mover": { "cache": {
                "contentCacheSize": { "percent": 80 },
                "metadataCacheSize": { "percent": 20 }
            } } } })),
        )
        .await
        .expect_err("budgets summing to 100% of capacity must be refused");
    assert!(
        err.to_string().contains("90%"),
        "the refusal must say why: {err}"
    );

    let _ = backups.delete(backup, &DeleteParams::default()).await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
}

/// The policy-owned persistent cache PVC (component label + owner reference).
async fn policy_cache_pvc(client: &Client, policy: &str) -> Option<PersistentVolumeClaim> {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    pvcs.list(&ListParams::default().labels("kopiur.home-operations.com/component=mover-cache"))
        .await
        .expect("list cache PVCs")
        .items
        .into_iter()
        .find(|p| {
            p.metadata.owner_references.as_ref().is_some_and(|o| {
                o.iter()
                    .any(|r| r.kind == "SnapshotPolicy" && r.name == policy)
            })
        })
}

fn requested_storage(pvc: &PersistentVolumeClaim) -> Option<String> {
    pvc.spec
        .as_ref()?
        .resources
        .as_ref()?
        .requests
        .as_ref()?
        .get("storage")
        .map(|q| q.0.clone())
}

/// Wait for an Event with `reason` regarding the SnapshotPolicy `policy`.
async fn wait_policy_event(client: &Client, policy: &str, reason: &str) -> Event {
    let events: Api<Event> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    wait_until(
        &format!("{reason} Event on SnapshotPolicy {policy}"),
        default_timeout(),
        poll_interval(),
        || async {
            let list = events.list(&ListParams::default()).await?;
            Ok(list.items.into_iter().find(|e| {
                e.reason.as_deref() == Some(reason)
                    && e.regarding.as_ref().is_some_and(|r| {
                        r.kind.as_deref() == Some("SnapshotPolicy")
                            && r.name.as_deref() == Some(policy)
                    })
            }))
        },
    )
    .await
    .unwrap_or_else(|e| panic!("expected a {reason} Event on {policy}: {e}"))
}

async fn set_capacity(policies: &Api<SnapshotPolicy>, policy: &str, capacity: &str) {
    policies
        .patch(
            policy,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "spec": { "mover": { "cache": { "capacity": capacity } } }
            })),
        )
        .await
        .expect("patch the policy's cache capacity");
}

async fn run_to_success(backups: &Api<Snapshot>, name: &str, policy: &str) {
    create_idempotent(
        backups,
        &cr(snapshot_json(
            E2E_NAMESPACE,
            name,
            policy,
            serde_json::json!({}),
        )),
        "create Snapshot",
    )
    .await;
    wait_phase(backups, name, "Succeeded")
        .await
        .unwrap_or_else(|e| panic!("{name} must succeed: {e}"));
}

/// #613: raising `capacity` on a `Persistent` cache used to leave the existing PVC
/// at its old size (a manual `kubectl edit pvc` was the only fix). The next run now
/// grows it when the StorageClass allows expansion (`csi-hostpath-sc` does), and a
/// smaller capacity is refused visibly instead of silently ignored.
#[tokio::test]
#[ignore = "requires the e2e harness + the CSI snapshot stack (mise run //crates/e2e:snapshot-stack)"]
async fn a_persistent_cache_pvc_grows_with_capacity_and_never_shrinks() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }
    let (repo, policy) = ("e2e-cg-repo", "e2e-cg-policy");
    let repos = ready_repo(&client, repo, "cachegrow").await;
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            E2E_NAMESPACE,
            policy,
            "Repository",
            repo,
            serde_json::json!({
                "mover": { "cache": {
                    "capacity": "64Mi",
                    "mode": "Persistent",
                    "storageClassName": CSI_STORAGE_CLASS
                } }
            }),
        )),
        "create SnapshotPolicy with a Persistent cache",
    )
    .await;

    // 1) The first run creates the warm cache PVC at 64Mi.
    run_to_success(&backups, "e2e-cg-backup-1", policy).await;
    let pvc = policy_cache_pvc(&client, policy)
        .await
        .expect("the Persistent cache PVC exists after the first run");
    assert_eq!(requested_storage(&pvc).as_deref(), Some("64Mi"));

    // 2) Raise capacity: the next run grows the claim (grow-only expansion).
    set_capacity(&policies, policy, "128Mi").await;
    run_to_success(&backups, "e2e-cg-backup-2", policy).await;
    let grown = policy_cache_pvc(&client, policy).await.expect("cache PVC");
    assert_eq!(
        requested_storage(&grown).as_deref(),
        Some("128Mi"),
        "raising mover.cache.capacity must grow the existing cache PVC"
    );
    wait_policy_event(&client, policy, "CachePvcExpanded").await;

    // 3) Lower capacity: Kubernetes cannot shrink a PVC. Refused, visibly, and the
    //    run still succeeds with budgets sized to the smaller request.
    set_capacity(&policies, policy, "32Mi").await;
    run_to_success(&backups, "e2e-cg-backup-3", policy).await;
    let kept = policy_cache_pvc(&client, policy).await.expect("cache PVC");
    assert_eq!(
        requested_storage(&kept).as_deref(),
        Some("128Mi"),
        "a smaller capacity must never be applied to the existing PVC"
    );
    let ev = wait_policy_event(&client, policy, "CachePvcShrinkRefused").await;
    assert_eq!(ev.type_.as_deref(), Some("Warning"));
    assert!(
        ev.note.as_deref().is_some_and(|n| n.contains("Fix:")),
        "the Warning must carry the fix: {:?}",
        ev.note
    );
    let pinned = resolved_cache(&backups, "e2e-cg-backup-3").await;
    assert_eq!(
        pinned["capacity"], "32Mi",
        "budgets follow the smaller requested capacity: {pinned}"
    );

    for b in ["e2e-cg-backup-1", "e2e-cg-backup-2", "e2e-cg-backup-3"] {
        let _ = backups.delete(b, &DeleteParams::default()).await;
    }
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
}
