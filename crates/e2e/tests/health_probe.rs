//! e2e: the backend health probe (`spec.health.probe`, default-on since #345).
//!
//! The wipe scenario proves the **`onFailure: Alert` opt-out contract**: after a
//! `Repository` is `Ready`, WIPING its backend out-of-band makes the probe raise
//! a `RepositoryVanished` alert — while the Alert-mode repository **stays
//! `Ready`** and is **never auto-recreated** (the pinned `uniqueId` is unchanged
//! and the backend stays empty). Never-recreate is the data-safety invariant the
//! whole design is built around, and it holds under EITHER `onFailure` mode —
//! under the default `Degrade` the same wipe instead opens the circuit breaker
//! (`Degraded`, then terminal `Failed` once the strict re-check confirms the
//! repository is gone); that arc is `crates/e2e/tests/repo_breaker.rs`.
//!
//! The last scenario in this file is the other half of that invariant (#435):
//! never-recreate is right, but a wiped repository still needs a documented way
//! back. A `Degrade`-mode `ClusterRepository` is wiped, parks terminal `Failed`
//! with `RepositoryReinitializeBlocked` (naming the exact `kubectl annotate`
//! command, and NOT the pre-#435 "set spec.create.enabled: true" — which was
//! already true), rejects a mismatched ack loudly, and re-initializes on the
//! right one.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::events::v1::Event as CoreEvent;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, ResourceExt};

use kopiur_api::{ClusterRepository, Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::builders::{self, SeedStep};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, consts, default_timeout, poll_interval, wait, wait_until,
};

mod common;

/// A `Repository` on a dedicated MinIO bucket with the health probe at a fast
/// cadence (`interval: 30s`, `failureThreshold: 1`) so the alert fires within
/// the e2e timeout, and `onFailure: Alert` — the wipe scenario tests the
/// alert-only OPT-OUT (the default is the `Degrade` circuit breaker since #345;
/// under it a wipe would escalate to terminal `Failed`, which
/// `repo_breaker.rs` covers). The churn guards below replace the whole
/// `health` block via [`churn_health_spec`], so this `onFailure` only shapes
/// the wipe scenario. `create.enabled: true` to prove Part A: even with create
/// on, a once-`Ready` repo is NEVER recreated on a vanish.
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
                "probe": {
                    "enabled": true,
                    "interval": "30s",
                    "failureThreshold": 1,
                    "onFailure": "Alert"
                }
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

/// The `onFailure: Alert` opt-out: a wiped backend raises `RepositoryVanished`
/// while the repository stays `Ready` (backups keep running) and is never
/// auto-recreated. Under the default `Degrade` mode the same wipe opens the
/// circuit breaker instead — see `repo_breaker.rs`.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn alert_mode_probe_alerts_on_wipe_but_stays_ready_and_never_recreates() {
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
        "phase must stay Ready under onFailure: Alert — the opt-out means a vanish never halts backups"
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
    // Wait specifically for a POST-Ready stamp: since #345 M3 the bootstrap
    // success SEEDS `lastProbeAt` (so a fresh repo doesn't probe immediately),
    // and the caller backdates that seed to arm the timer — both of those
    // values predate `ready_at`, so "first value present" would read the seed,
    // not the probe. Only a probe that connected after Ready satisfies this.
    let probed_at = wait_until(
        "status.health.lastProbeAt is stamped by a post-Ready probe",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(status_now()
                .await
                .pointer("/health/lastProbeAt")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc))
                .filter(|d| *d >= ready_at))
        },
    )
    .await
    .expect(
        "a probe must FINALIZE and stamp status.health.lastProbeAt past Ready — it is the \
         only input to the probe timer, so an unstamped probe re-fires forever (#273)",
    );
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

    // #345 M3 seeds `lastProbeAt` at bootstrap success, so a freshly-Ready
    // repository deliberately does NOT probe immediately (no fleet-wide
    // probe-Job storm at upgrade) — but this guard needs a probe to actually
    // run inside CHURN_WINDOW. Backdate the seed past the 10m interval so the
    // timer is due NOW; the probe must then launch, finalize, and re-stamp a
    // post-Ready `lastProbeAt` (the #273 contract this test pins).
    let backdated =
        (ready_at - chrono::Duration::seconds(CHURN_PROBE_INTERVAL_SECS as i64 + 60)).to_rfc3339();
    repos
        .patch_status(
            name,
            &PatchParams::default(),
            &Patch::Merge(
                serde_json::json!({ "status": { "health": { "lastProbeAt": backdated } } }),
            ),
        )
        .await
        .expect("backdate the seeded lastProbeAt to arm the probe timer");

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

    // Same seed-backdating as the namespaced twin: arm the probe timer so a
    // probe actually runs inside CHURN_WINDOW (see the comment there).
    let backdated =
        (ready_at - chrono::Duration::seconds(CHURN_PROBE_INTERVAL_SECS as i64 + 60)).to_rfc3339();
    repos
        .patch_status(
            name,
            &PatchParams::default(),
            &Patch::Merge(
                serde_json::json!({ "status": { "health": { "lastProbeAt": backdated } } }),
            ),
        )
        .await
        .expect("backdate the seeded lastProbeAt to arm the probe timer");

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

// ---------------------------------------------------------------------------
// #435: deliberate re-initialize after a wipe.
//
// The reported failure: a user's bucket was deleted, and the once-`Ready`
// repository parked telling them to "set spec.create.enabled: true" — which was
// already true. There was no documented way back, so they deleted the whole
// namespace. This scenario is the arc end to end: wipe → an accurate terminal
// reason carrying the exact `kubectl annotate` command → a WRONG ack ignored
// loudly → the RIGHT ack re-initializes and backups resume.
//
// The "ack goes inert once the pin rotates" property is unit-tested
// (`health::create_gate_truth_table`), not e2e'd: a second Degrade→Failed cycle
// would need another ~120s strict-retry holdoff and blow the per-wait budget.
// ---------------------------------------------------------------------------

/// The re-initialize scenario's `ClusterRepository`: the DEFAULT
/// `onFailure: Degrade` (so a wipe escalates through the breaker to terminal
/// `Failed`, unlike the Alert-mode opt-out above), `create.enabled: true` — the
/// whole point, since #435 is about a user whose create WAS enabled — and a fast
/// probe so the wipe is noticed inside the budget.
fn reinit_repository_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "ClusterRepository",
        "metadata": { "name": name },
        "spec": {
            "backend": { "s3": {
                "bucket": consts::BUCKET_REINIT_ACK,
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
            "health": {
                "probe": { "enabled": true, "interval": "30s", "failureThreshold": 1 }
            }
        }
    })
}

/// An Event with `reason` regarding `kind`/`name`, or `None`. Listed
/// cluster-wide: a `ClusterRepository` has no namespace of its own, so where the
/// Recorder files its Events is an implementation detail this assertion must not
/// depend on.
async fn find_event(
    events: &Api<CoreEvent>,
    kind: &str,
    name: &str,
    reason: &str,
) -> Option<CoreEvent> {
    events
        .list(&ListParams::default())
        .await
        .ok()?
        .items
        .into_iter()
        .find(|e| {
            e.reason.as_deref() == Some(reason)
                && e.regarding.as_ref().is_some_and(|r| {
                    r.kind.as_deref() == Some(kind) && r.name.as_deref() == Some(name)
                })
        })
}

#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn degrade_mode_wipe_escalates_to_reinitialize_blocked_and_ack_recreates() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem, Need::Minio])
        .await
        .expect("provision the source PVC + MinIO");
    let client = world.client().clone();

    let name = "e2e-reinit-ack";
    let policy = "e2e-reinit-ack-policy";
    let repos: Api<ClusterRepository> = Api::all(client.clone());
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let snaps: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let events: Api<CoreEvent> = Api::all(client.clone());

    clear_leftover(&repos, name).await;
    repos
        .create(
            &PostParams::default(),
            &serde_json::from_value(reinit_repository_json(name))
                .expect("ClusterRepository JSON deserializes"),
        )
        .await
        .expect("create ClusterRepository");

    // 1. First bootstrap creates the repository and pins U1, and one backup
    //    lands real history in it — the history the re-initialize will discard,
    //    which is exactly why kopiur refuses to do it silently.
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
    let u1 = cluster_status_value(&repos.get(name).await.unwrap())
        .get("uniqueId")
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .expect("a Ready repository pins a uniqueId");

    let _ = policies
        .create(
            &PostParams::default(),
            &common::cr(common::snapshot_policy_json(
                E2E_NAMESPACE,
                policy,
                "ClusterRepository",
                name,
                serde_json::json!({}),
            )),
        )
        .await;
    snaps
        .create(
            &PostParams::default(),
            &common::cr(common::snapshot_json(
                E2E_NAMESPACE,
                "e2e-reinit-ack-snap-1",
                policy,
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create the pre-wipe Snapshot");
    common::wait_phase(&snaps, "e2e-reinit-ack-snap-1", "Succeeded")
        .await
        .expect("a backup succeeds before the wipe");

    // 2. Wipe the bucket out-of-band. The bucket SURVIVES (it is the kopia
    //    repository that vanished, not the storage), so the acknowledged
    //    re-create later has somewhere to land — which is the real-world shape
    //    too: an object-lifecycle rule, or an errant `mc rm --recursive`.
    let wipe = builders::foreign_kopia_pod(
        E2E_NAMESPACE,
        "e2e-reinit-ack-wipe",
        &[SeedStep::WipeBucket {
            bucket: consts::BUCKET_REINIT_ACK,
        }],
    );
    pods.create(&PostParams::default(), &wipe)
        .await
        .expect("create wipe pod");
    wait::pod_succeeded(&client, E2E_NAMESPACE, "e2e-reinit-ack-wipe")
        .await
        .expect("wipe pod empties the bucket");

    // 3. Degrade → (one strict-retry holdoff, ~120s) → terminal `Failed` with
    //    the ACCURATE reason. Pre-#435 this said "spec.create.enabled is false,
    //    set it to true" on a repository whose create was already enabled.
    wait_until(
        &format!("{name} Failed/RepositoryReinitializeBlocked"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = cluster_status_value(&repos.get(name).await?);
            let blocked = s.get("phase").and_then(|p| p.as_str()) == Some("Failed")
                && condition(&s, "Ready", "reason").as_deref()
                    == Some("RepositoryReinitializeBlocked");
            Ok(blocked.then_some(()))
        },
    )
    .await
    .expect("a wiped once-Ready repository parks at RepositoryReinitializeBlocked");

    let blocked_msg = condition(
        &cluster_status_value(&repos.get(name).await.unwrap()),
        "Ready",
        "message",
    )
    .expect("the Ready condition carries a message");
    assert!(
        blocked_msg.contains(&format!("allow-reinitialize={u1}")),
        "the condition must name the exact annotation value (this is what \
         `kubectl kopiur status` prints verbatim), got: {blocked_msg}"
    );
    assert!(
        !blocked_msg.contains("spec.create.enabled is false"),
        "THE #435 BUG: create.enabled is true here, so this advice is false. \
         Got: {blocked_msg}"
    );
    assert!(
        !blocked_msg.contains(" -n "),
        "a ClusterRepository is cluster-scoped: the command must carry no -n. \
         Got: {blocked_msg}"
    );

    // ...and the same reason reaches `kubectl get events`, with the annotate
    // command intact after the apiserver's 1024-byte note clamp (the command
    // leads the message precisely so truncation cannot eat it).
    let ev = wait_until(
        "a RepositoryReinitializeBlocked Warning Event is published",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(find_event(
                &events,
                "ClusterRepository",
                name,
                "RepositoryReinitializeBlocked",
            )
            .await)
        },
    )
    .await
    .expect("the block must be visible as a Warning Event, not only in the condition");
    let note = ev.note.unwrap_or_default();
    assert!(note.len() <= 1024, "Event note is {} bytes", note.len());
    assert!(
        note.contains(&format!("allow-reinitialize={u1}")),
        "the annotate command must survive the note clamp, got: {note}"
    );

    // 4. A WRONG ack is ignored (fail-safe) and says so. Without this the user
    //    sees nothing at all and concludes kopiur dropped their annotation.
    repos
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "metadata": { "annotations": {
                    "kopiur.home-operations.com/allow-reinitialize": "definitely-not-the-pin"
                }}
            })),
        )
        .await
        .expect("annotate with a wrong ack value");
    wait_until(
        "an InvalidReinitializeAck Warning Event is published",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(find_event(&events, "ClusterRepository", name, "InvalidReinitializeAck").await)
        },
    )
    .await
    .expect("a mismatched ack must be reported, not silently ignored");
    assert_eq!(
        cluster_status_value(&repos.get(name).await.unwrap())
            .get("phase")
            .and_then(|p| p.as_str()),
        Some("Failed"),
        "a mismatched ack must NOT re-initialize anything"
    );

    // 5. The RIGHT ack: re-initialize. The phase may pass through `Degraded`
    //    rather than `Initializing` (the breaker was open — `health::launch_phase`
    //    deliberately does not flap a Degraded repository), so wait on `Ready`.
    repos
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "metadata": { "annotations": {
                    "kopiur.home-operations.com/allow-reinitialize": u1
                }}
            })),
        )
        .await
        .expect("annotate with the pinned uniqueId");
    wait_until(
        &format!("{name} Ready again after the acknowledged re-initialize"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = cluster_status_value(&repos.get(name).await?);
            Ok((s.get("phase").and_then(|p| p.as_str()) == Some("Ready")).then_some(()))
        },
    )
    .await
    .expect("an acknowledged re-initialize must bring the repository back to Ready");

    let healed = cluster_status_value(&repos.get(name).await.unwrap());
    let u2 = healed
        .get("uniqueId")
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .expect("the re-initialized repository pins a uniqueId");
    assert_ne!(
        u2, u1,
        "a re-initialize MINTS a new repository, so the pin must rotate — and that \
         rotation is what makes the still-applied ack inert against a future wipe"
    );
    assert_eq!(
        condition(&healed, "BackendReachable", "status").as_deref(),
        Some("True"),
        "the strict success must heal the #345 breaker (no second reset path exists)"
    );

    // 6. Backups work again — the point of the whole exercise.
    snaps
        .create(
            &PostParams::default(),
            &common::cr(common::snapshot_json(
                E2E_NAMESPACE,
                "e2e-reinit-ack-snap-2",
                policy,
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create the post-re-initialize Snapshot");
    common::wait_phase(&snaps, "e2e-reinit-ack-snap-2", "Succeeded")
        .await
        .expect("a backup succeeds into the re-initialized repository");

    // 7. The ack is still applied (value U1) and the pin is now U2 — the exact
    //    state a GitOps manifest lands in after a successful re-initialize. It
    //    must be INERT AND SILENT: no `InvalidReinitializeAck` naming U1, or the
    //    reward for following the documented procedure is a permanent Warning on
    //    a healthy repository. The step-4 Warning (value "definitely-not-the-pin",
    //    published while Failed) legitimately remains, so match on the note.
    let stale_ack_warning = events
        .list(&ListParams::default())
        .await
        .expect("list events")
        .items
        .into_iter()
        .find(|e| {
            e.reason.as_deref() == Some("InvalidReinitializeAck")
                && e.regarding.as_ref().is_some_and(|r| {
                    r.kind.as_deref() == Some("ClusterRepository")
                        && r.name.as_deref() == Some(name)
                })
                && e.note
                    .as_deref()
                    .is_some_and(|n| n.contains(&format!("is `{u1}`")))
        });
    assert!(
        stale_ack_warning.is_none(),
        "a once-valid ack left behind after a successful re-initialize must be \
         inert AND silent on a Ready repository; got: {stale_ack_warning:?}"
    );

    let _ = snaps
        .delete("e2e-reinit-ack-snap-1", &DeleteParams::default())
        .await;
    let _ = snaps
        .delete("e2e-reinit-ack-snap-2", &DeleteParams::default())
        .await;
    let _ = policies.delete(policy, &DeleteParams::default()).await;
    let _ = repos.delete(name, &DeleteParams::default()).await;
}
