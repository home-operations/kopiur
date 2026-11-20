//! e2e (#760): the life of a `mode: Persistent` mover cache PVC after its first
//! run. The parts that delete things, which a mock cannot prove.
//!
//! 1. **Scrub**: a mover that mounts a cache PVC from an older release removes
//!    the kopia configuration file (which holds the backend credentials in
//!    plain text), the staged credential files and the logs from it, and keeps
//!    the cache.
//! 2. **Reset under a running mover**: a cache reset never deletes the PVC from
//!    under a mover Job. The PVC is retired, a run that starts meanwhile uses a
//!    per-run volume, and the PVC goes after the Job that mounts it finishes.
//!
//! The sweep of central cache PVCs is in `central_execution.rs`, beside the
//! central fixtures it needs.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster. Runs in the
//! `cache-budgets` shard (`csi: true`): the cache PVC is on `csi-hostpath-sc`.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use k8s_openapi::api::events::v1::Event;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client, ResourceExt};

use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::slow_mover::{MoverOp, SlowMover, with_slow_mover_config};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

/// The annotation the controller retires a cache PVC with.
const RETIRED: &str = "kopiur.home-operations.com/cache-retired-at";
/// The annotation that requests a cache reset on a `SnapshotPolicy`.
const RESET: &str = "kopiur.home-operations.com/cache-reset-requested-at";

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

/// A `SnapshotPolicy` in the operator namespace with a 64Mi `Persistent` cache.
async fn persistent_policy(policies: &Api<SnapshotPolicy>, policy: &str, repo: &str) {
    create_idempotent(
        policies,
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
}

/// Delete `names` and wait until none of them exists.
async fn gone<K>(api: &Api<K>, names: &[&str], what: &str)
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    for name in names {
        let _ = api.delete(name, &DeleteParams::default()).await;
    }
    wait_until(what, default_timeout(), poll_interval(), || async {
        for name in names {
            if api.get_opt(name).await?.is_some() {
                return Ok(None);
            }
        }
        Ok(Some(()))
    })
    .await
    .unwrap_or_else(|e| panic!("{what}: {e}"));
}

/// Run a one-shot busybox pod that mounts `claim` at `/mnt` in the operator
/// namespace, wait for it to succeed, and delete it again.
async fn on_claim(client: &Client, pod: &str, claim: &str, script: &str, what: &str) {
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    gone(&pods, &[pod], "a previous helper pod gone").await;
    pods.create(
        &PostParams::default(),
        &kopiur_e2e::builders::one_shot_pod(
            E2E_NAMESPACE,
            pod,
            &["sh", "-c", script],
            &[(claim, "/mnt")],
        ),
    )
    .await
    .expect("create the helper pod");
    kopiur_e2e::wait::pod_succeeded(client, E2E_NAMESPACE, pod)
        .await
        .unwrap_or_else(|e| panic!("{what}: {e}"));
    gone(&pods, &[pod], "the helper pod gone").await;
}

/// kopia writes the backend credentials in plain text into its configuration
/// file, and releases up to 0.11.1 kept that file, the staged credential files
/// and the logs on the cache volume. A `Persistent` cache PVC outlives every
/// pod, so those files stayed on it between runs. The first mover of this
/// release that mounts such a PVC must remove them, and must keep the cache:
/// the layout of the cache on the volume did not change.
#[tokio::test]
#[ignore = "requires the e2e harness + the CSI snapshot stack (mise run //crates/e2e:snapshot-stack)"]
async fn a_mover_scrubs_the_files_an_older_release_left_on_a_persistent_cache_pvc() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }
    let (repo, policy, backup) = ("e2e-cs-repo", "e2e-cs-policy", "e2e-cs-backup");
    // An in-namespace policy keeps its cache in `kopiur-cache-<policy>`.
    let claim = format!("kopiur-cache-{policy}");
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // A retry of this test finds the previous attempt's objects; start clean.
    gone(&backups, &[backup], "a previous Snapshot gone").await;
    gone(&policies, &[policy], "a previous SnapshotPolicy gone").await;
    kopiur_e2e::pvc::delete_pvc_and_consumers(&client, E2E_NAMESPACE, &claim)
        .await
        .expect("clear a previous cache PVC");
    let repos = ready_repo(&client, repo, "cachescrub").await;

    // The cache PVC as a mover of an older release left it: everything at the
    // root of the volume, owned by the mover's uid.
    pvcs.create(
        &PostParams::default(),
        &cr(serde_json::json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": { "name": claim, "namespace": E2E_NAMESPACE },
            "spec": {
                "accessModes": ["ReadWriteOnce"],
                "storageClassName": CSI_STORAGE_CLASS,
                "resources": { "requests": { "storage": "64Mi" } },
            },
        })),
    )
    .await
    .expect("create the old cache PVC");
    on_claim(
        &client,
        "e2e-cs-seed",
        &claim,
        "set -e; mkdir -p /mnt/creds /mnt/logs /mnt/cache; \
         echo '{\"storage\":{\"config\":{\"secretAccessKey\":\"leaked\"}}}' > /mnt/repository.config; \
         echo '{}' > /mnt/repository.config.update-info.json; \
         echo key > /mnt/creds/sftp_key; echo log > /mnt/logs/kopia.log; \
         echo warm > /mnt/cache/kopiur-e2e-marker; chown -R 65532:65532 /mnt",
        "seed the cache PVC with the files of an older mover",
    )
    .await;

    persistent_policy(&policies, policy, repo).await;
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
    wait_phase(&backups, backup, "Succeeded")
        .await
        .expect("a backup over an old cache PVC should succeed");
    let status = status_json(&backups, backup).await;
    assert_eq!(
        status["resolved"]["cache"]["claim"]["name"].as_str(),
        Some(claim.as_str()),
        "the mover must have mounted the existing cache PVC: {status}"
    );

    on_claim(
        &client,
        "e2e-cs-check",
        &claim,
        "ls -A /mnt /mnt/cache; \
         test ! -e /mnt/repository.config && test ! -e /mnt/repository.config.update-info.json \
         && test ! -e /mnt/creds && test ! -e /mnt/logs \
         && test \"$(cat /mnt/cache/kopiur-e2e-marker)\" = warm",
        "the old configuration, credential and log files must be gone, and the cache kept",
    )
    .await;

    gone(&backups, &[backup], "the Snapshot gone").await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    // The test made this PVC itself, with no owner, so nothing else deletes it.
    let _ = pvcs.delete(&claim, &DeleteParams::default()).await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
}

/// The claim names a mover Job's pod template mounts.
async fn mounted_claims(jobs: &Api<Job>, job: &str) -> anyhow::Result<Vec<String>> {
    Ok(jobs
        .get(job)
        .await?
        .spec
        .and_then(|s| s.template.spec)
        .and_then(|s| s.volumes)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.persistent_volume_claim.map(|c| c.claim_name))
        .collect())
}

/// Kubernetes only protects a PVC from deletion while a SCHEDULED pod mounts
/// it. A cache reset that deleted the PVC directly could take it from under a
/// mover: a pod that is not scheduled yet, or the next pod of a Job with
/// retries left, then waits for a PVC that does not exist. So a reset retires
/// the PVC first and deletes it only when no unfinished mover Job mounts it.
///
/// The slow-mover fixture holds one backup Job open for longer than the grace
/// period, so the reset meets a PVC that a Job still mounts.
#[tokio::test]
#[ignore = "requires the e2e harness + the CSI snapshot stack (mise run //crates/e2e:snapshot-stack)"]
async fn a_cache_reset_waits_for_the_mover_job_that_mounts_the_pvc() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    if !csi_class_present_or_skip(&client, CSI_STORAGE_CLASS).await {
        return;
    }
    let (repo, policy) = ("e2e-cr-repo", "e2e-cr-policy");
    let (holder, bystander) = ("e2e-cr-holder", "e2e-cr-bystander");
    let claim = format!("kopiur-cache-{policy}");
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let events: Api<Event> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    gone(&backups, &[holder, bystander], "previous Snapshots gone").await;
    gone(&policies, &[policy], "a previous SnapshotPolicy gone").await;
    kopiur_e2e::pvc::delete_pvc_and_consumers(&client, E2E_NAMESPACE, &claim)
        .await
        .expect("clear a previous cache PVC");
    let repos = ready_repo(&client, repo, "cachereset").await;
    persistent_policy(&policies, policy, repo).await;
    let policy_uid = policies
        .get(policy)
        .await
        .expect("get SnapshotPolicy")
        .uid()
        .expect("policy uid");

    // Long enough to outlast the 60s grace period and the reset's next retry.
    let hold = Duration::from_secs(180);
    let config = SlowMover::new(hold).ops(&[MoverOp::Snapshot]);
    let outcome = with_slow_mover_config(&world, config, || async {
        let run = |name: &'static str| {
            let backups = backups.clone();
            async move {
                create_idempotent(
                    &backups,
                    &cr(snapshot_json(
                        E2E_NAMESPACE,
                        name,
                        policy,
                        serde_json::json!({}),
                    )),
                    "create Snapshot",
                )
                .await;
                wait_phase(&backups, name, "Running").await
            }
        };
        let phase = |name: &'static str| {
            let backups = backups.clone();
            async move {
                status_json(&backups, name).await["phase"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            }
        };

        // 1) A slow backup creates the cache PVC and holds it.
        run(holder).await?;
        let created = pvcs.get(&claim).await?;
        let claim_uid = created.uid().unwrap_or_default();
        anyhow::ensure!(
            mounted_claims(&jobs, holder).await?.contains(&claim),
            "the first mover must mount the persistent cache PVC"
        );

        // 2) Request a reset while that Job runs. The PVC is retired at once.
        policies
            .patch(
                policy,
                &PatchParams::default(),
                &Patch::Merge(&serde_json::json!({
                    "metadata": { "annotations": { RESET: "2026-07-06T12:00:00Z" } }
                })),
            )
            .await?;
        wait_until(
            "the cache PVC retired",
            default_timeout(),
            poll_interval(),
            || async {
                Ok(pvcs
                    .get_opt(&claim)
                    .await?
                    .is_some_and(|c| c.annotations().contains_key(RETIRED))
                    .then_some(()))
            },
        )
        .await?;
        let retired_seen = tokio::time::Instant::now();
        wait_until(
            "CacheResetPending Event on the SnapshotPolicy",
            default_timeout(),
            poll_interval(),
            || async {
                let list = events.list(&ListParams::default()).await?;
                Ok(list
                    .items
                    .into_iter()
                    .any(|e| {
                        e.reason.as_deref() == Some("CacheResetPending")
                            && e.regarding
                                .as_ref()
                                .is_some_and(|r| r.uid.as_deref() == Some(policy_uid.as_str()))
                    })
                    .then_some(()))
            },
        )
        .await?;

        // 3) A run that starts now must not mount the retired PVC.
        run(bystander).await?;
        anyhow::ensure!(
            !mounted_claims(&jobs, bystander).await?.contains(&claim),
            "a run that starts after the reset request must use a per-run volume"
        );
        let bystander_status = status_json(&backups, bystander).await;
        anyhow::ensure!(
            bystander_status["resolved"]["cache"]["claim"].is_null(),
            "the bystander must record no persistent cache PVC: {bystander_status}"
        );

        // 4) Past the grace period and the reset's retries, the holder still
        //    runs, so the PVC must still be there: retired, and not deleted.
        tokio::time::sleep_until(retired_seen + Duration::from_secs(100)).await;
        anyhow::ensure!(
            phase(holder).await == "Running",
            "the scenario needs the holder still running here; raise the hold"
        );
        let kept = pvcs.get_opt(&claim).await?;
        anyhow::ensure!(
            kept.as_ref().is_some_and(|c| {
                c.uid().as_deref() == Some(claim_uid.as_str())
                    && c.metadata.deletion_timestamp.is_none()
            }),
            "the cache PVC must not be deleted while a mover Job mounts it: {:?}",
            kept.map(|c| c.metadata)
        );

        // 5) The holder finishes, and with it the wait: the PVC is deleted and
        //    the request is recorded as honored.
        wait_phase(&backups, holder, "Succeeded").await?;
        wait_until(
            "the cache PVC deleted after its mover Job finished",
            default_timeout(),
            poll_interval(),
            || async { Ok(pvcs.get_opt(&claim).await?.is_none().then_some(())) },
        )
        .await?;
        // Kubernetes keeps a deleted PVC while a pod object references it, so
        // the PVC only goes because the finished mover pod went. Its Job stays.
        anyhow::ensure!(
            jobs.get_opt(holder).await?.is_some(),
            "the finished mover Job must stay; only its pod is deleted"
        );
        wait_until(
            "the reset request recorded as honored",
            default_timeout(),
            poll_interval(),
            || async {
                let status = status_json(&policies, policy).await;
                Ok((status["cache"]["resetRequestHonored"].as_str()
                    == Some("2026-07-06T12:00:00Z"))
                .then_some(()))
            },
        )
        .await?;
        wait_phase(&backups, bystander, "Succeeded").await?;
        Ok(())
    })
    .await;
    outcome.expect("the reset must wait for the mover Job, then delete the cache PVC");

    gone(&backups, &[holder, bystander], "the Snapshots gone").await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
}
