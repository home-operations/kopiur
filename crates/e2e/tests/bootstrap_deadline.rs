//! e2e: deadline-killed bootstrap/probe Jobs (#413, #414, #415).
//!
//! One incident, three bugs: a cold-cache `kopia repository connect` outgrew
//! the 120s bootstrap Job deadline, and the operator (a) misreported the kill
//! as `BackendUnreachable` (#414), (b) withheld maintenance — the cure — from
//! the resulting `Degraded` repository (#413), and (c) relaunched the doomed
//! discovery Job every ~2.5 minutes forever, billed per request (#415).
//!
//! The fixture is a deliberately impossible `activeDeadlineSeconds: 1` against
//! a HEALTHY MinIO — the deterministic way to make every connect die by
//! deadline while the backend answers instantly, i.e. exactly the
//! "slow, not down" shape the fixes classify.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use k8s_openapi::api::batch::v1::Job;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::{Api, ResourceExt};

use kopiur_api::{Maintenance, Repository};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, consts, default_timeout, poll_interval, wait_until};

/// How long the #415 guard watches for a premature relaunch. The strict-retry
/// holdoff gates the second attempt for ≥120s from the failure's `lastProbeAt`
/// stamp; on the pre-fix code the relaunch landed within seconds of the
/// finalize, so 90s of "no new Job UID" cleanly separates the two without
/// waiting out a full backoff rung (the full 120→240→480→960→1800 ladder is
/// pinned by the unit sequence test `recycled_bootstrap_failures_rearm_the_holdoff`).
const HOLDOFF_GUARD_WINDOW: Duration = Duration::from_secs(90);

fn repository_json(name: &str, bucket: &str, deadline_secs: Option<i64>) -> serde_json::Value {
    let mut spec = serde_json::json!({
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
        // failureThreshold 1: the FIRST deadline kill crosses the breaker
        // threshold, so the reason/condition assertions don't wait out a
        // 3-failure debounce.
        "health": { "probe": { "enabled": true, "interval": "30s", "failureThreshold": 1 } }
    });
    if let Some(secs) = deadline_secs {
        spec["bootstrap"] =
            serde_json::json!({ "failurePolicy": { "activeDeadlineSeconds": secs } });
    }
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": spec
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

/// Sample the `<name>-discovery` Job's uid every [`poll_interval`] for `window`,
/// returning every DISTINCT uid seen (the `health_probe.rs` churn-guard pattern:
/// under-sampling can only make the guard MORE forgiving, never flakily red).
async fn distinct_discovery_job_uids(
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

/// #414 + #413 on a BOOTSTRAPPED repository: a spec edit drops the bootstrap
/// deadline to an impossible 1s, so the strict re-bootstrap is deadline-killed
/// against a healthy backend. The kill must be classified as a deadline kill
/// (`ProbeDeadlineExceeded` on `BackendReachable`/`Ready`,
/// `BootstrapDeadlineExceeded` on `Bootstrapped` — NOT the old
/// "unreachable/credentials" text), and the repository's managed `Maintenance`
/// must keep working (#413: no `WaitingForRepository` deferral — a manual run
/// SUCCEEDS against the degraded-but-reachable repository, because the
/// maintenance mover's own deadline is the 48h default, not the probe's).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn deadline_killed_relaunch_degrades_with_the_deadline_reason_and_maintenance_still_runs() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio])
        .await
        .expect("provision MinIO + buckets");
    let client = world.client().clone();
    let bucket = "kopiur-bootstrap-deadline-a";
    let repo = "e2e-deadline-a";

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let maints: Api<Maintenance> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    clear_leftover(&repos, repo).await;

    // 1. Bootstrap normally to Ready (default 120s deadline is plenty for
    //    in-cluster MinIO) and pin the uniqueId + the managed Maintenance.
    repos
        .create(
            &PostParams::default(),
            &serde_json::from_value(repository_json(repo, bucket, None))
                .expect("Repository JSON deserializes"),
        )
        .await
        .expect("create Repository");
    wait_until(
        &format!("{repo} Ready with a uniqueId"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_value(&repos.get(repo).await?);
            let ready = s.get("phase").and_then(|p| p.as_str()) == Some("Ready")
                && s.get("uniqueId").and_then(|u| u.as_str()).is_some();
            Ok(ready.then_some(()))
        },
    )
    .await
    .expect("repository bootstraps to Ready");
    wait_until(
        &format!("managed Maintenance {repo} exists"),
        default_timeout(),
        poll_interval(),
        || async { Ok(maints.get_opt(repo).await?.map(|_| ())) },
    )
    .await
    .expect("a Ready repository projects its managed Maintenance");

    // 2. Drop the bootstrap deadline to an impossible 1s. The generation bump
    //    forces a strict re-bootstrap, which the Job controller deadline-kills
    //    while MinIO stays healthy — the exact "slow, not down" shape.
    repos
        .patch(
            repo,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "spec": { "bootstrap": { "failurePolicy": { "activeDeadlineSeconds": 1 } } }
            })),
        )
        .await
        .expect("patch activeDeadlineSeconds");

    // 3. #414: the degradation names the DEADLINE, not a phantom outage.
    wait_until(
        &format!("{repo} Degraded with the deadline reasons"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_value(&repos.get(repo).await?);
            let classified = s.get("phase").and_then(|p| p.as_str()) == Some("Degraded")
                && condition(&s, "BackendReachable", "reason").as_deref()
                    == Some("ProbeDeadlineExceeded")
                && condition(&s, "Bootstrapped", "reason").as_deref()
                    == Some("BootstrapDeadlineExceeded");
            Ok(classified.then_some(()))
        },
    )
    .await
    .expect(
        "#414: a deadline kill must degrade with ProbeDeadlineExceeded/BootstrapDeadlineExceeded",
    );
    let s = status_value(&repos.get(repo).await.unwrap());
    let msg = condition(&s, "BackendReachable", "message").unwrap_or_default();
    assert!(
        msg.contains("activeDeadlineSeconds"),
        "#414: the alert must name the deadline and its fix, got: {msg}"
    );
    assert!(
        !msg.contains("credentials/lock failed"),
        "#414: the alert must NOT send the operator chasing credential ghosts: {msg}"
    );

    // 4. #413: maintenance is EXEMPT for a deadline-degraded repository — the
    //    managed Maintenance must not defer WaitingForRepository, and a manual
    //    quick run must actually SUCCEED (its mover connects fine: only the
    //    1s bootstrap deadline is impossible, the backend is healthy).
    let requested = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    maints
        .patch(
            repo,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "metadata": { "annotations": {
                kopiur_api::consts::RUN_REQUESTED_ANNOTATION: requested,
                kopiur_api::consts::RUN_MODE_ANNOTATION: "quick",
            }}})),
        )
        .await
        .expect("annotate Maintenance with a manual run request");
    wait_until(
        &format!("manual maintenance {requested} succeeds against the Degraded repository"),
        default_timeout(),
        poll_interval(),
        || async {
            let m = maints.get(repo).await?;
            let s = serde_json::to_value(&m)
                .ok()
                .and_then(|v| v.get("status").cloned())
                .unwrap_or_default();
            // The #413 deadlock, as a live assertion: the old gate wrote
            // LeaseOwned=False/WaitingForRepository here and never spawned.
            if let Some(reason) = condition(&s, "LeaseOwned", "reason")
                && reason == "WaitingForRepository"
            {
                panic!(
                    "#413 deadlock: maintenance deferred WaitingForRepository against a \
                     Degraded-because-slow repository — maintenance is the cure and must run"
                );
            }
            let done = s.pointer("/manualRun/requestedAt").and_then(|v| v.as_str())
                == Some(requested.as_str())
                && s.pointer("/manualRun/phase").and_then(|v| v.as_str()) == Some("Succeeded");
            Ok(done.then_some(()))
        },
    )
    .await
    .expect("#413: maintenance must run (and succeed) against a deadline-degraded repository");

    // Cleanup (best-effort).
    let _ = repos.delete(repo, &DeleteParams::default()).await;
}

/// #415 + deadline escalation on a NEVER-bootstrapped repository born with an
/// impossible 1s deadline: the first kill must arm the strict-retry holdoff
/// (no relaunch inside [`HOLDOFF_GUARD_WINDOW`] — pre-fix, a fresh cold-cache
/// connect launched within seconds, ~24 billed attempts/hour), the gate must
/// then RELEASE (no wedge), and the second attempt must run with the ESCALATED
/// 2s deadline (the operator applies the "raise activeDeadlineSeconds"
/// remediation itself).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn first_deadline_kill_arms_the_relaunch_holdoff_and_escalates_the_deadline() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Minio])
        .await
        .expect("provision MinIO + buckets");
    let client = world.client().clone();
    let bucket = "kopiur-bootstrap-deadline-b";
    let repo = "e2e-deadline-b";
    let job_name = format!("{repo}-discovery");

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    clear_leftover(&repos, repo).await;

    repos
        .create(
            &PostParams::default(),
            &serde_json::from_value(repository_json(repo, bucket, Some(1)))
                .expect("Repository JSON deserializes"),
        )
        .await
        .expect("create Repository");

    // 1. The first attempt dies by deadline: Degraded, streak 1 stamped (#415's
    //    prerequisite — the old code never wrote status.health here), and the
    //    Ready condition names the deadline (#414's first-bootstrap flavor).
    wait_until(
        &format!("{repo} Degraded with consecutiveProbeFailures=1"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_value(&repos.get(repo).await?);
            let armed = s.get("phase").and_then(|p| p.as_str()) == Some("Degraded")
                && s.pointer("/health/consecutiveProbeFailures")
                    .and_then(|v| v.as_i64())
                    == Some(1);
            Ok(armed.then_some(()))
        },
    )
    .await
    .expect("#415: the first result-less failure must stamp the failure streak");
    let s = status_value(&repos.get(repo).await.unwrap());
    assert_eq!(
        condition(&s, "Ready", "reason").as_deref(),
        Some("BootstrapDeadlineExceeded"),
        "#414: a first-bootstrap deadline kill carries its own reason; status: {s}"
    );
    let msg = condition(&s, "Ready", "message").unwrap_or_default();
    assert!(
        msg.contains("activeDeadlineSeconds"),
        "#414: the message must name the deadline and its fix: {msg}"
    );
    let first_uids = distinct_discovery_job_uids(&jobs, &job_name, Duration::from_secs(5)).await;

    // 2. THE #415 ASSERTION: for the guard window, NO new discovery Job may
    //    appear — the holdoff gates the relaunch for ≥120s from the failure.
    //    Pre-fix, the recycle route never stamped the holdoff's anchor, so a
    //    fresh Job (and a fresh billed cold-cache connect) landed within
    //    seconds of the finalize.
    let during = distinct_discovery_job_uids(&jobs, &job_name, HOLDOFF_GUARD_WINDOW).await;
    let new_during: BTreeSet<_> = during.difference(&first_uids).collect();
    assert!(
        new_during.is_empty(),
        "#415 relaunch metronome: {} new discovery Job(s) inside the {HOLDOFF_GUARD_WINDOW:?} \
         holdoff window — the failure streak/anchor is not gating the relaunch. New UIDs: \
         {new_during:?}",
        new_during.len()
    );

    // 3. The gate RELEASES (no wedge): the second attempt arrives once the
    //    120s rung elapses, and it runs with the ESCALATED 2s deadline.
    wait_until(
        &format!("{job_name} second attempt with the escalated deadline"),
        default_timeout(),
        poll_interval(),
        || async {
            let Some(job) = jobs.get_opt(&job_name).await? else {
                return Ok(None);
            };
            let is_new = job.uid().is_some_and(|u| !during.contains(&u));
            if !is_new {
                return Ok(None);
            }
            Ok(job.spec.as_ref().and_then(|s| s.active_deadline_seconds))
        },
    )
    .await
    .map(|deadline| {
        assert_eq!(
            deadline, 2,
            "deadline escalation: the second attempt must run with base<<1 = 2s"
        );
    })
    .expect("the holdoff must release and relaunch with an escalated deadline");

    // Cleanup (best-effort).
    let _ = repos.delete(repo, &DeleteParams::default()).await;
}
