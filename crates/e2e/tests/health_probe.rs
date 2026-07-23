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

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, PostParams};
use kube::{Api, ResourceExt};

use kopiur_api::{ClusterRepository, Repository};
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

    // 1b. #273: the probe must actually FIRE, and keep firing on its 30s cadence.
    //     The churn guards below prove the bootstrap Job stops being recreated; this
    //     proves that was not achieved by simply never probing again. On the buggy
    //     code `lastProbeAt` was never written at all, because the recycle branch
    //     returned before `finalize_*` — the only writer of it.
    let read_last_probe_at = || async {
        Ok(status_value(&repos.get(repo).await?)
            .pointer("/health/lastProbeAt")
            .and_then(|v| v.as_str())
            .map(str::to_string))
    };
    let first_probe_at = wait_until(
        &format!("{repo} status.health.lastProbeAt is stamped"),
        default_timeout(),
        poll_interval(),
        read_last_probe_at,
    )
    .await
    .expect("the probe must finalize and stamp lastProbeAt (#273)");
    wait_until(
        &format!("{repo} status.health.lastProbeAt advances"),
        default_timeout(),
        poll_interval(),
        || async { Ok(read_last_probe_at().await?.filter(|t| *t != first_probe_at)) },
    )
    .await
    .expect("the probe must re-fire on its 30s cadence, not stamp once and stop");

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

// ---------------------------------------------------------------------------
// #273 — the bootstrap-Job churn guard.
//
// `probe_due` was derived from `status.health.lastProbeAt`, but the finished-Job
// branch deleted the Job and returned BEFORE `finalize_*` — the only writer of
// `lastProbeAt`. So the gate destroyed every Job whose completion would have
// cleared it: the `<name>-discovery` Job was recreated every ~15-25s forever,
// `status.health` stayed permanently empty, and `BackendReachable` was never
// written. Only Job-based backends (object stores, server, volume-backed
// filesystem) were affected; bare-path filesystem repos connect in-process.
//
// The fix is a launch-side attempt stamp (`status.health.probeAttemptAt`), so
// these guards assert BOTH halves of the contract: the Job stops churning, AND
// the probe still actually completes and publishes. A "fix" that simply stopped
// probing would pass the first assertion and fail the rest.
// ---------------------------------------------------------------------------

/// Probe cadence for the churn guards.
///
/// Deliberately far longer than [`CHURN_WINDOW`]: a CORRECT operator legitimately
/// creates and destroys one mover Job *per interval*, so the assertion has to
/// separate "one Job per interval" from "a Job every few seconds". At the 30s
/// webhook floor (`crates/api/src/validate/repository.rs`) the correct rate
/// (1/30s) and the buggy rate (~1/20s) differ by under 2x and the guard would be a
/// coin flip. At 10m they differ by ~30x.
const CHURN_PROBE_INTERVAL: &str = "10m";
const CHURN_PROBE_INTERVAL_SECS: u64 = 600;

/// How long the `<name>-discovery` Job is watched, measured from `Ready`.
const CHURN_WINDOW: Duration = Duration::from_secs(180);

/// Distinct `<name>-discovery` Job UIDs a CORRECT operator may produce inside
/// [`CHURN_WINDOW`]:
///
/// ```text
///   ceil(window / interval)   periodic probes that may legitimately fire
/// + 1                         the FIRST probe, always immediately due:
///                             `refresh_due(None, ..)` is true until
///                             `status.health.lastProbeAt` is first stamped
/// + 1                         the initial bootstrap Job, still present when the
///                             window opens (`finalize_*` deletes it only for a
///                             probe run)
/// ```
///
/// = `ceil(180/600) + 2` = 3. A correct run observes exactly 2; the buggy code
/// produces 9-12 and is still climbing when the window closes.
const MAX_BOOTSTRAP_JOB_UIDS: usize =
    (CHURN_WINDOW.as_secs() as usize).div_ceil(CHURN_PROBE_INTERVAL_SECS as usize) + 2;

/// The `spec.health` block shared by both churn guards.
fn churn_health_spec() -> serde_json::Value {
    serde_json::json!({
        "probe": { "enabled": true, "interval": CHURN_PROBE_INTERVAL, "failureThreshold": 1 }
    })
}

/// Sample the `<name>-discovery` Job's `metadata.uid` every [`poll_interval`] for
/// `window`, returning every DISTINCT uid seen.
///
/// A missing Job (the gap between a delete and the next create) and a transient API
/// error both count as "no sample". Under-sampling can only LOWER the count, so it
/// can never turn a correct run red — it only makes the guard more forgiving of the
/// bug, never the reverse.
async fn distinct_bootstrap_job_uids(
    jobs: &Api<Job>,
    job_name: &str,
    window: Duration,
) -> BTreeSet<String> {
    let deadline = Instant::now() + window;
    let mut seen = BTreeSet::new();
    while Instant::now() < deadline {
        if let Ok(Some(job)) = jobs.get_opt(job_name).await
            && let Some(uid) = job.uid()
        {
            seen.insert(uid);
        }
        tokio::time::sleep(poll_interval()).await;
    }
    seen
}

/// Delete a leftover CR of the same name and wait for it to go (reused clusters).
async fn clear_leftover<K>(api: &Api<K>, name: &str)
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    if api.get_opt(name).await.expect("query leftover").is_some() {
        let _ = api.delete(name, &DeleteParams::default()).await;
        wait_until(
            &format!("leftover {name} is gone"),
            default_timeout(),
            poll_interval(),
            || async { Ok(api.get_opt(name).await?.is_none().then_some(())) },
        )
        .await
        .expect("leftover CR should delete");
    }
}

/// Assert the shared post-`Ready` contract: bounded Job churn, a stamped
/// `lastProbeAt` that post-dates `Ready`, `BackendReachable=True`, and an
/// unchanged `Ready` phase.
async fn assert_probe_finalizes_without_churn(
    jobs: &Api<Job>,
    job_name: &str,
    ready_at: chrono::DateTime<chrono::Utc>,
    status_now: impl AsyncFn() -> serde_json::Value,
) {
    // THE #273 ASSERTION. Buggy: ~9-12 UIDs. Fixed: exactly 2.
    let uids = distinct_bootstrap_job_uids(jobs, job_name, CHURN_WINDOW).await;
    assert!(
        uids.len() <= MAX_BOOTSTRAP_JOB_UIDS,
        "#273 bootstrap-Job hot-loop: {} distinct {job_name} Jobs in {CHURN_WINDOW:?} at \
         probe interval {CHURN_PROBE_INTERVAL} (bound {MAX_BOOTSTRAP_JOB_UIDS}) — the probe \
         is recycling its Job without ever finalizing it, so status.health.lastProbeAt is \
         never stamped and the probe stays permanently due. UIDs: {uids:?}",
        uids.len()
    );

    // ... and it must be a probe that actually RAN, not a probe switched off. Both of
    // these are written only by the finalize path, which is unreachable on buggy code.
    let probed_at = wait_until(
        "status.health.lastProbeAt is stamped",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(status_now()
                .await
                .pointer("/health/lastProbeAt")
                .and_then(|v| v.as_str())
                .map(str::to_string))
        },
    )
    .await
    .expect(
        "a probe must FINALIZE and stamp status.health.lastProbeAt — it is the only input \
         to the probe timer, so an unstamped probe re-fires forever (#273)",
    );
    let probed_at = chrono::DateTime::parse_from_rfc3339(&probed_at)
        .expect("lastProbeAt is RFC 3339")
        .with_timezone(&chrono::Utc);
    assert!(
        probed_at >= ready_at,
        "lastProbeAt {probed_at} predates Ready ({ready_at}) — that is the initial \
         bootstrap being re-read, not a probe that connected"
    );

    let s = status_now().await;
    assert_eq!(
        condition(&s, "BackendReachable", "status").as_deref(),
        Some("True"),
        "a completed healthy probe must publish BackendReachable=True; status: {s}"
    );
    assert_eq!(
        s.get("phase").and_then(|p| p.as_str()),
        Some("Ready"),
        "a healthy probe must never move the phase"
    );
}

/// #273: a probe-enabled `Repository` on an object store must FINALIZE its probe
/// instead of recycling the `<name>-discovery` Job forever.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn probe_finalizes_instead_of_recreating_the_bootstrap_job() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio])
        .await
        .expect("provision MinIO + buckets");
    let client = world.client().clone();

    let name = "e2e-probe-churn";
    let job_name = format!("{name}-discovery");
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    clear_leftover(&repos, name).await;
    let mut spec = probe_repository_json(name, consts::BUCKET_PROBE_CHURN_REPO);
    spec["spec"]["health"] = churn_health_spec();
    repos
        .create(
            &PostParams::default(),
            &serde_json::from_value(spec).expect("Repository JSON deserializes"),
        )
        .await
        .expect("create Repository");

    wait_until(
        &format!("{name} Ready"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_value(&repos.get(name).await?);
            Ok((s.get("phase").and_then(|p| p.as_str()) == Some("Ready")).then_some(()))
        },
    )
    .await
    .expect("repository becomes Ready");
    let ready_at = chrono::Utc::now();

    assert_probe_finalizes_without_churn(&jobs, &job_name, ready_at, async || {
        status_value(&repos.get(name).await.expect("get Repository"))
    })
    .await;

    let _ = repos.delete(name, &DeleteParams::default()).await;
}

/// #273 on the kind the issue was actually reported against. `cluster_repository.rs`
/// is a hand-copied twin of `repository.rs`, so the guard must cover both or the
/// reported path stays unguarded.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn cluster_repository_probe_finalizes_instead_of_recreating_the_bootstrap_job() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio])
        .await
        .expect("provision MinIO + buckets");
    let client = world.client().clone();

    let name = "e2e-probe-churn-crepo";
    let job_name = format!("{name}-discovery");
    let repos: Api<ClusterRepository> = Api::all(client.clone());
    // A ClusterRepository's bootstrap Job lands in the credential Secret's namespace,
    // which the spec below pins to E2E_NAMESPACE.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    clear_leftover(&repos, name).await;
    let cr = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "ClusterRepository",
        "metadata": { "name": name },
        "spec": {
            "backend": { "s3": {
                "bucket": consts::BUCKET_PROBE_CHURN_CREPO,
                "endpoint": consts::MINIO_ENDPOINT,
                "region": "us-east-1",
                "tls": { "disableTls": true },
                "auth": { "secretRef": { "name": consts::SECRET_S3_CREDS, "namespace": E2E_NAMESPACE } }
            }},
            "encryption": {
                "passwordSecretRef": {
                    "name": consts::SECRET_S3_CREDS,
                    "namespace": E2E_NAMESPACE,
                    "key": "KOPIA_PASSWORD"
                }
            },
            "create": { "enabled": true },
            "allowedNamespaces": { "all": true },
            "maintenance": { "enabled": false },
            "health": churn_health_spec(),
        }
    });
    repos
        .create(
            &PostParams::default(),
            &serde_json::from_value(cr).expect("ClusterRepository JSON deserializes"),
        )
        .await
        .expect("create ClusterRepository");

    wait_until(
        &format!("{name} Ready"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = cluster_status_value(&repos.get(name).await?);
            Ok((s.get("phase").and_then(|p| p.as_str()) == Some("Ready")).then_some(()))
        },
    )
    .await
    .expect("cluster repository becomes Ready");
    let ready_at = chrono::Utc::now();

    assert_probe_finalizes_without_churn(&jobs, &job_name, ready_at, async || {
        cluster_status_value(&repos.get(name).await.expect("get ClusterRepository"))
    })
    .await;

    let _ = repos.delete(name, &DeleteParams::default()).await;
}

fn cluster_status_value(repo: &ClusterRepository) -> serde_json::Value {
    serde_json::to_value(repo)
        .ok()
        .and_then(|v| v.get("status").cloned())
        .unwrap_or_default()
}
