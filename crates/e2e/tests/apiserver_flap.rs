//! API-server-flap resilience e2e (the 2026-07 EMFILE incident): the control
//! plane gets OOM-killed repeatedly, and the operator must SURVIVE it — no fd
//! exhaustion, no runaway crash/lease churn — and converge once it returns.
//!
//! ## What this is (and is not)
//!
//! This is a resilience guard, not a strict fail-without-fix reproducer: a
//! kind-scale fleet may not reach EMFILE at the default NOFILE limit even on
//! the pre-fix code. The strict regression tests are hermetic (the reconcile
//! concurrency cap, transport-error Event suppression, bounded publishes,
//! requeue jitter, client timeouts, the renew-attempt deadline). What this
//! scenario DOES catch is a reintroduced crash-loop, any `Too many open files`
//! in the controller log, runaway lease transitions, and a controller that
//! fails to reconcile after recovery.
//!
//! ## Shard isolation (load-bearing)
//!
//! One sequential scenario; the binary OWNS its CI shard — it kills the
//! kube-apiserver process on the kind node (via `kopiur_e2e::flap_apiserver`),
//! which no other test may race. Host-side disruption from a scenario has
//! precedent (mass_deletion flips its repo dir read-only mid-test).

#![cfg(all(unix, feature = "e2e"))]

use std::time::Duration;

use kube::Api;
use kube::api::{LogParams, PostParams};

use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::Pod;

use kopiur_api::{Repository, Snapshot, SnapshotPolicy, SnapshotSchedule};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, consts, default_timeout, flap_apiserver, poll_interval, wait_until,
};

mod common;
use common::{cr, wait_phase};

/// The election Lease name (the chart sets `KOPIUR_LEASE_NAME` to the release
/// fullname; the harness installs release `kopiur`).
const LEASE_NAME: &str = "kopiur";
/// Controller pod name prefix (`<fullname>-controller-<hash>-<rand>`).
const CONTROLLER_POD_PREFIX: &str = "kopiur-controller-";
/// How many times the apiserver is killed, and the gap between kills — long
/// enough for the static pod to be mid-restart (a half-alive control plane),
/// short enough that the flap looks like the incident's OOM loop.
const KILLS: u32 = 3;
const KILL_GAP: Duration = Duration::from_secs(20);
/// Fleet size seeded before the flap so the recovery re-list + referent
/// fan-out actually exercises the storm path (policy → schedule mappers).
const FLEET: usize = 60;

fn repository_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "backend": { "filesystem": { "path": "/repo", "volume": { "pvc": { "name": consts::PVC_REPO } } } },
            "encryption": { "passwordSecretRef": { "name": "kopia-creds", "key": "KOPIA_PASSWORD" } },
            "create": { "enabled": true }
        }
    })
}

fn policy_json(name: &str, repo: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": repo },
            "sources": [ { "pvc": { "name": consts::PVC_SRC } } ],
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 }
        }
    })
}

/// A schedule that never fires during the test (yearly cron, no runOnCreate):
/// the fleet exists to feed the watch fan-out + re-list volume, not to launch
/// sixty backups.
fn schedule_json(name: &str, policy: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotSchedule",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "policyRef": { "name": policy },
            "schedule": { "cron": "0 0 1 1 *" }
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

/// The controller pod (name, total container restartCount), if one exists.
async fn controller_pod(pods: &Api<Pod>) -> Result<Option<(String, i32)>, kube::Error> {
    let list = pods.list(&Default::default()).await?;
    Ok(list
        .items
        .iter()
        .filter(|p| {
            p.metadata
                .name
                .as_deref()
                .unwrap_or("")
                .starts_with(CONTROLLER_POD_PREFIX)
        })
        .map(|p| {
            let restarts = p
                .status
                .as_ref()
                .and_then(|s| s.container_statuses.as_ref())
                .map(|cs| cs.iter().map(|c| c.restart_count).sum())
                .unwrap_or(0);
            (p.metadata.name.clone().unwrap_or_default(), restarts)
        })
        .next())
}

async fn lease_transitions(leases: &Api<Lease>) -> Result<i32, kube::Error> {
    Ok(leases
        .get_opt(LEASE_NAME)
        .await?
        .and_then(|l| l.spec)
        .and_then(|s| s.lease_transitions)
        .unwrap_or(0))
}

/// Controller logs: the CURRENT container's log is required (retried through
/// post-flap apiserver blips — an empty string here would make the EMFILE
/// assertion vacuously pass), the PREVIOUS instance's log is best-effort (it
/// errors when the container never restarted; a by-design abdication restart
/// must not hide an EMFILE line when it exists).
async fn controller_logs(pods: &Api<Pod>, pod: &str) -> anyhow::Result<String> {
    let current = wait_until(
        "controller current-container logs are readable",
        default_timeout(),
        poll_interval(),
        || async {
            let chunk = pods.logs(pod, &LogParams::default()).await?;
            // The controller logs its startup banner immediately; an empty
            // body means kubelet is still wiring the log stream — retry.
            Ok((!chunk.is_empty()).then_some(chunk))
        },
    )
    .await?;
    let mut all = current;
    if let Ok(prev) = pods
        .logs(
            pod,
            &LogParams {
                previous: true,
                ..Default::default()
            },
        )
        .await
    {
        all.push('\n');
        all.push_str(&prev);
    }
    Ok(all)
}

#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn controller_survives_an_apiserver_flap_and_converges() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client();

    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let leases: Api<Lease> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // -- healthy baseline ---------------------------------------------------
    repos
        .create(&PostParams::default(), &cr(repository_json("flap-repo")))
        .await
        .expect("create Repository");
    wait_phase(&repos, "flap-repo", "Ready")
        .await
        .expect("Repository must reach Ready before the flap");

    // Seed the fleet so the post-flap re-list + policy→schedule fan-out has
    // real volume (the storm path the incident amplified).
    for i in 0..FLEET {
        let p = format!("flap-pol-{i}");
        policies
            .create(&PostParams::default(), &cr(policy_json(&p, "flap-repo")))
            .await
            .expect("create fleet SnapshotPolicy");
        schedules
            .create(
                &PostParams::default(),
                &cr(schedule_json(&format!("flap-sched-{i}"), &p)),
            )
            .await
            .expect("create fleet SnapshotSchedule");
    }

    let baseline_transitions = lease_transitions(&leases)
        .await
        .expect("read baseline lease transitions");
    let baseline = controller_pod(&pods)
        .await
        .expect("list controller pods")
        .expect("a controller pod exists");
    eprintln!(
        "[flap] baseline: pod={} restarts={} lease_transitions={baseline_transitions}",
        baseline.0, baseline.1
    );

    // -- the flap -------------------------------------------------------------
    flap_apiserver(KILLS, KILL_GAP)
        .await
        .expect("flap the kind apiserver");

    // The apiserver comes back (kubelet restarts the static pod); wait_until
    // tolerates the dead-window polls. Require SUSTAINED health — several
    // consecutive successful probes — not one lucky answer: a restarting
    // apiserver serves a request and drops the next ("tls handshake eof"),
    // and the assertions below must run against a stable control plane.
    let consecutive_ok = std::sync::atomic::AtomicU32::new(0);
    wait_until(
        "apiserver stably answering after the flap",
        default_timeout(),
        poll_interval(),
        || async {
            use std::sync::atomic::Ordering;
            match client.apiserver_version().await {
                Ok(_) => {
                    let n = consecutive_ok.fetch_add(1, Ordering::Relaxed) + 1;
                    Ok((n >= 5).then_some(()))
                }
                Err(e) => {
                    consecutive_ok.store(0, Ordering::Relaxed);
                    Err(e)
                }
            }
        },
    )
    .await
    .expect("apiserver must recover to sustained health");

    // -- survival assertions --------------------------------------------------
    // (a) The controller pod is Running with a bounded restart count. Restarts
    // are LEGAL here — leader election abdicates by design after >10s without
    // a renew — but the incident signature was a runaway loop (transitions=80),
    // so the bound is what matters: at most ~one abdication per kill + one for
    // luck, and Running at the end.
    let max_restart_delta = (KILLS as i32) + 1;
    let (pod_name, _) = wait_until(
        "controller pod Running with bounded restarts",
        default_timeout(),
        poll_interval(),
        || async {
            let Some((name, restarts)) = controller_pod(&pods).await? else {
                return Ok(None);
            };
            let delta = if name == baseline.0 {
                restarts - baseline.1
            } else {
                // The pod was replaced (eviction/redeploy): all its restarts
                // are post-flap.
                restarts
            };
            assert!(
                delta <= max_restart_delta,
                "controller restarted {delta} times for {KILLS} apiserver kills (max \
                 {max_restart_delta}) — crash-loop regression"
            );
            let running = pods
                .get(&name)
                .await?
                .status
                .and_then(|s| s.phase)
                .is_some_and(|p| p == "Running");
            Ok(running.then_some((name, restarts)))
        },
    )
    .await
    .expect("controller pod must be Running after the flap");

    // (b) NO fd exhaustion, current or previous container instance. The log
    // fetch retries through residual blips and requires a non-empty current
    // log — an unreadable log must fail the test, not vacuously pass it.
    let logs = controller_logs(&pods, &pod_name)
        .await
        .expect("controller logs must be readable after recovery");
    assert!(
        !logs.contains("Too many open files"),
        "controller hit fd exhaustion (EMFILE) during the apiserver flap"
    );

    // (c) Lease churn stays bounded: one takeover per abdication, not the
    // incident's runaway counter. Read through wait_until — a residual blip
    // on this one GET must not fail the scenario.
    let transitions = wait_until(
        "post-flap lease transitions readable",
        default_timeout(),
        poll_interval(),
        || async { lease_transitions(&leases).await.map(Some) },
    )
    .await
    .expect("read post-flap lease transitions");
    let delta = transitions - baseline_transitions;
    assert!(
        delta <= (KILLS as i32) * 2 + 1,
        "lease changed hands {delta} times for {KILLS} kills — leadership churn regression \
         (baseline {baseline_transitions}, now {transitions})"
    );

    // (d) The operator actually WORKS after recovery: a fresh backup runs to
    // Succeeded with a real kopia snapshot id — the assertion that would hang
    // forever on a controller that survived but stopped reconciling.
    // Create through a blip-tolerant retry; a 409 means an earlier attempt
    // landed before its response was lost — success.
    wait_until(
        "post-flap Snapshot created",
        default_timeout(),
        poll_interval(),
        || async {
            match backups
                .create(
                    &PostParams::default(),
                    &cr(snapshot_json("flap-proof", "flap-pol-0")),
                )
                .await
            {
                Ok(_) => Ok(Some(())),
                Err(kube::Error::Api(e)) if e.code == 409 => Ok(Some(())),
                Err(e) => Err(e),
            }
        },
    )
    .await
    .expect("create post-flap Snapshot");
    wait_phase(&backups, "flap-proof", "Succeeded")
        .await
        .expect("post-flap Snapshot must reach Succeeded");
    let proof = backups
        .get("flap-proof")
        .await
        .expect("get post-flap Snapshot");
    let snap_id = serde_json::to_value(&proof)
        .ok()
        .and_then(|v| {
            v.pointer("/status/snapshot/kopiaSnapshotID")
                .and_then(|s| s.as_str().map(String::from))
        })
        .unwrap_or_default();
    assert!(
        !snap_id.is_empty(),
        "post-flap Snapshot Succeeded but carries no kopiaSnapshotID"
    );
    eprintln!("[flap] converged: post-flap backup {snap_id}; restarts+transitions bounded");
}
