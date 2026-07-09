//! e2e regression guards for the **work-spec ConfigMap leak** (#224, the "605
//! ConfigMaps" report): mover runs used to apply a work-spec `ConfigMap` +
//! `Job` pair, owner-referenced to a long-lived CR. The Job self-reaped via
//! `ttlSecondsAfterFinished`, but nothing ever deleted the ConfigMap — it lived
//! as long as its owner (a `Snapshot` CR is the durable backup record, a
//! `SnapshotPolicy`/`RepositoryReplication` is permanent), so one ConfigMap
//! accumulated per run, forever.
//!
//! The fix is structural, and both halves are guarded here:
//! 1. **No per-run ConfigMap exists at all** — the work spec rides the mover
//!    Job's own pod env, so a run is exactly ONE object whose TTL cleans up
//!    everything.
//! 2. **Orphan sweep** — a leader-only periodic pass reaps the LEGACY
//!    work-spec ConfigMaps left by pre-fix operator versions. The harness
//!    values (`deploy/e2e/values.yaml`) run it on a fast cadence.
//!
//! Plus the pin-consumption bug the audit surfaced: a stale terminal
//! `{name}-pin` Job satisfied the next pin toggle's "Job succeeded" check, so
//! the controller recorded `status.pinned = desired` WITHOUT running a mover —
//! kopia's real pin state silently diverged.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;

use std::time::Duration;

use common::{cr, wait_phase};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::Api;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};

use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, consts, default_timeout, poll_interval, wait_until};

/// The repository password Secret the chart-installed operator reads.
const CREDS_SECRET: &str = "kopia-creds";

/// How long a reaped ConfigMap must STAY gone under reconcile pokes. The
/// regression shape is a requeue/watch-event loop re-creating it, which fires
/// within seconds.
const QUIET_WINDOW: Duration = Duration::from_secs(30);

/// Provision the shared filesystem repo + a policy for `policy_name`, and wait
/// the Repository Ready. `extra_policy` is merged into the SnapshotPolicy spec.
async fn ensure_repo_and_policy(
    client: &kube::Client,
    repo_name: &str,
    policy_name: &str,
    extra_policy: serde_json::Value,
) {
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let repo = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": repo_name, "namespace": E2E_NAMESPACE },
        "spec": {
            "backend": { "filesystem": { "path": "/repo", "volume": { "pvc": { "name": "kopiur-e2e-repo" } } } },
            "encryption": { "passwordSecretRef": { "name": CREDS_SECRET, "key": "KOPIA_PASSWORD" } },
            "create": { "enabled": true }
        }
    });
    let _ = repos.create(&PostParams::default(), &cr(repo)).await;
    wait_phase(&repos, repo_name, "Ready")
        .await
        .expect("repository should reach Ready");

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let mut spec = serde_json::json!({
        "repository": { "kind": "Repository", "name": repo_name },
        "sources": [ { "pvc": { "name": "e2e-src" } } ],
        // e2e-src is a statically-provisioned (non-CSI) hostPath PVC; copyMethod
        // defaults to Snapshot, which would fail preflight against it.
        "copyMethod": "Direct"
        // NO mover.ttlSecondsAfterFinished: the DEFAULT 1h TTL applies, so the
        // finished Job outlives the whole test — proving the ConfigMap reap is
        // the controller's own transition-time delete, not TTL/owner GC.
    });
    if let Some(extra) = extra_policy.as_object() {
        for (k, v) in extra {
            spec[k] = v.clone();
        }
    }
    let policy = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": policy_name, "namespace": E2E_NAMESPACE },
        "spec": spec
    });
    let _ = policies.create(&PostParams::default(), &cr(policy)).await;
}

/// Create a Snapshot (deleting any leftover of the same name first) and return
/// its Api handle.
async fn create_snapshot(
    client: &kube::Client,
    name: &str,
    policy_name: &str,
    extra_spec: serde_json::Value,
) -> Api<Snapshot> {
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    if backups
        .get_opt(name)
        .await
        .expect("query leftover Snapshot")
        .is_some()
    {
        let _ = backups.delete(name, &DeleteParams::default()).await;
        wait_until(
            &format!("leftover {name} is gone"),
            default_timeout(),
            poll_interval(),
            || async { Ok(backups.get_opt(name).await?.is_none().then_some(())) },
        )
        .await
        .expect("leftover Snapshot should delete (finalizer included)");
    }
    let mut spec = serde_json::json!({
        "policyRef": { "name": policy_name },
        "deletionPolicy": "Retain"
    });
    if let Some(extra) = extra_spec.as_object() {
        for (k, v) in extra {
            spec[k] = v.clone();
        }
    }
    let backup = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": spec
    });
    backups
        .create(&PostParams::default(), &cr(backup))
        .await
        .expect("create Snapshot");
    backups
}

/// Assert the run named `name` has NO same-named ConfigMap — the work spec
/// rides the Job env, so a per-run ConfigMap existing at any point is the
/// leak regression.
async fn assert_no_work_spec_cm(cms: &Api<ConfigMap>, name: &str, when: &str) {
    assert!(
        cms.get_opt(name).await.expect("get ConfigMap").is_none(),
        "run {name} has a per-run work-spec ConfigMap ({when}) — the spec must ride the Job env"
    );
}

/// Assert no ConfigMap named `name` appears during [`QUIET_WINDOW`].
async fn assert_cm_stays_gone(cms: &Api<ConfigMap>, name: &str, while_doing: &str) {
    let deadline = tokio::time::Instant::now() + QUIET_WINDOW;
    while tokio::time::Instant::now() < deadline {
        assert_no_work_spec_cm(cms, name, while_doing).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

// ---------------------------------------------------------------------------
// 1. Succeeded backup: no per-run ConfigMap exists at any point; the Job (its
//    env carries the whole spec) is left to its TTL.
// ---------------------------------------------------------------------------

/// THE leak report: one work-spec ConfigMap per (hourly) backup, forever. The
/// fix is structural — the spec rides the mover Job's pod env, so no per-run
/// ConfigMap is ever created, while the Job (default 1h TTL) still carries the
/// full controller→mover contract and the pod logs.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn succeeded_backup_creates_no_work_spec_cm_and_keeps_job() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    ensure_repo_and_policy(
        &client,
        "e2e-leak-repo",
        "e2e-leak-cfg",
        serde_json::json!({}),
    )
    .await;
    let backups = create_snapshot(
        &client,
        "e2e-leak-ok",
        "e2e-leak-cfg",
        serde_json::json!({}),
    )
    .await;
    wait_phase(&backups, "e2e-leak-ok", "Succeeded")
        .await
        .expect("Snapshot should reach Succeeded");

    // No per-run ConfigMap was created; the Job (default 1h TTL) is present
    // and carries the work spec inline.
    assert_no_work_spec_cm(&cms, "e2e-leak-ok", "after Succeeded").await;
    let job = jobs
        .get_opt("e2e-leak-ok")
        .await
        .expect("get mover Job")
        .expect("the succeeded mover Job must persist to its TTL");
    let has_spec_env = job
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|p| p.containers.first())
        .and_then(|c| c.env.as_ref())
        .is_some_and(|env| env.iter().any(|e| e.name == "KOPIUR_WORK_SPEC"));
    assert!(
        has_spec_env,
        "the mover Job must carry the inline work-spec env"
    );

    // Poke the Snapshot (any watch event re-reconciles it): no ConfigMap may
    // appear — a terminal run must not re-apply mover objects.
    let poke = serde_json::json!({
        "metadata": { "annotations": { "kopiur-e2e/leak-poke": "1" } }
    });
    backups
        .patch("e2e-leak-ok", &PatchParams::default(), &Patch::Merge(&poke))
        .await
        .expect("poke Snapshot");
    assert_cm_stays_gone(&cms, "e2e-leak-ok", "poking a Succeeded Snapshot").await;

    // And the phase is untouched (no accidental re-run).
    let after = backups
        .get_opt("e2e-leak-ok")
        .await
        .expect("get Snapshot")
        .expect("Snapshot exists");
    let phase = serde_json::to_value(&after).ok().and_then(|v| {
        v.pointer("/status/phase")
            .and_then(|p| p.as_str().map(String::from))
    });
    assert_eq!(phase.as_deref(), Some("Succeeded"));
}

// ---------------------------------------------------------------------------
// 2. Failed backup: no per-run ConfigMap either (manual Failed Snapshots are
//    never pruned, so a ConfigMap would leak unbounded); Job kept for logs.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn failed_backup_creates_no_work_spec_cm_and_keeps_job() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // Deterministic mover failure: an unknown kopia flag, one attempt only.
    ensure_repo_and_policy(
        &client,
        "e2e-leak-fail-repo",
        "e2e-leak-fail-cfg",
        serde_json::json!({ "extraArgs": ["--kopiur-e2e-bogus-flag"] }),
    )
    .await;
    let backups = create_snapshot(
        &client,
        "e2e-leak-fail",
        "e2e-leak-fail-cfg",
        serde_json::json!({ "failurePolicy": { "backoffLimit": 0 } }),
    )
    .await;
    wait_phase(&backups, "e2e-leak-fail", "Failed")
        .await
        .expect("poisoned Snapshot should end Failed");

    assert_no_work_spec_cm(&cms, "e2e-leak-fail", "after Failed").await;
    assert!(
        jobs.get_opt("e2e-leak-fail")
            .await
            .expect("get mover Job")
            .is_some(),
        "the FAILED mover Job must be kept (debugging contract: pod logs until its TTL)"
    );
    assert_cm_stays_gone(
        &cms,
        "e2e-leak-fail",
        "waiting after a Failed Snapshot went terminal",
    )
    .await;
}

// ---------------------------------------------------------------------------
// 3. Pin toggle: a consumed pin Job means the next toggle runs a FRESH mover.
// ---------------------------------------------------------------------------

/// The silent-divergence bug: the `{name}-pin` Job was never deleted, so a
/// pin→unpin toggle found the stale SUCCEEDED Job, matched "Job succeeded", and
/// recorded `status.pinned = desired` without running a mover — kopia stayed
/// pinned while the CR said otherwise. The fixed controller consumes the
/// terminal pin Job (+ ConfigMap) once its result is recorded, so a toggle must
/// observably spawn a NEW pin Job.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn pin_toggle_after_success_runs_a_fresh_pin_job() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    ensure_repo_and_policy(
        &client,
        "e2e-leak-repo",
        "e2e-leak-cfg",
        serde_json::json!({}),
    )
    .await;
    let backups = create_snapshot(
        &client,
        "e2e-leak-pin",
        "e2e-leak-cfg",
        serde_json::json!({}),
    )
    .await;
    wait_phase(&backups, "e2e-leak-pin", "Succeeded")
        .await
        .expect("Snapshot should reach Succeeded");

    // Pin it.
    let pin = serde_json::json!({ "spec": { "pin": true } });
    backups
        .patch("e2e-leak-pin", &PatchParams::default(), &Patch::Merge(&pin))
        .await
        .expect("patch spec.pin = true");
    wait_until(
        "status.pinned == true",
        default_timeout(),
        poll_interval(),
        || async {
            let obj = backups.get_opt("e2e-leak-pin").await?;
            let pinned = obj
                .and_then(|o| serde_json::to_value(&o).ok())
                .and_then(|v| v.pointer("/status/pinned").and_then(|p| p.as_bool()));
            Ok((pinned == Some(true)).then_some(()))
        },
    )
    .await
    .expect("the pin mover must record status.pinned = true");

    // Regression assert #1: the terminal pin Job is CONSUMED once its result
    // is recorded. On the buggy code it lingers for its TTL (1h) — this wait
    // times out.
    wait_until(
        "terminal pin Job consumed",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(jobs
                .get_opt("e2e-leak-pin-pin")
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("the succeeded pin Job must be consumed after status.pinned is recorded");
    assert_no_work_spec_cm(&cms, "e2e-leak-pin-pin", "after pin-Job consumption").await;

    // Unpin — and require a FRESH pin Job to appear while the flip completes.
    // On the buggy code (stale Job satisfying the success check) the flip
    // happens with NO Job ever created; here the stale Job is gone, so a
    // recorded flip without a new Job is impossible — but observe it anyway so
    // the failure mode is named, not inferred.
    let unpin = serde_json::json!({ "spec": { "pin": false } });
    backups
        .patch(
            "e2e-leak-pin",
            &PatchParams::default(),
            &Patch::Merge(&unpin),
        )
        .await
        .expect("patch spec.pin = false");
    let deadline = tokio::time::Instant::now() + default_timeout();
    let mut saw_fresh_pin_job = false;
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!("status.pinned never flipped to false after unpin");
        }
        if !saw_fresh_pin_job
            && jobs
                .get_opt("e2e-leak-pin-pin")
                .await
                .ok()
                .flatten()
                .is_some()
        {
            saw_fresh_pin_job = true;
        }
        let pinned = backups
            .get_opt("e2e-leak-pin")
            .await
            .ok()
            .flatten()
            .and_then(|o| serde_json::to_value(&o).ok())
            .and_then(|v| v.pointer("/status/pinned").and_then(|p| p.as_bool()));
        if pinned == Some(false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        saw_fresh_pin_job,
        "pin divergence regression: status.pinned flipped to false without a fresh pin mover \
         Job — the controller recorded a pin state kopia never applied"
    );
}

// ---------------------------------------------------------------------------
// 4. Restore: same contract as backups — no per-run ConfigMap; Job to its TTL.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn completed_restore_creates_no_work_spec_cm_and_keeps_job() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client = world.client().clone();
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    ensure_repo_and_policy(
        &client,
        "e2e-leak-repo",
        "e2e-leak-cfg",
        serde_json::json!({}),
    )
    .await;
    let backups = create_snapshot(
        &client,
        "e2e-leak-restore-src",
        "e2e-leak-cfg",
        serde_json::json!({}),
    )
    .await;
    wait_phase(&backups, "e2e-leak-restore-src", "Succeeded")
        .await
        .expect("source Snapshot should reach Succeeded");

    let restores: Api<kopiur_api::Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    if restores
        .get_opt("e2e-leak-restore")
        .await
        .expect("query leftover Restore")
        .is_some()
    {
        let _ = restores
            .delete("e2e-leak-restore", &DeleteParams::default())
            .await;
        wait_until(
            "leftover e2e-leak-restore is gone",
            default_timeout(),
            poll_interval(),
            || async {
                Ok(restores
                    .get_opt("e2e-leak-restore")
                    .await?
                    .is_none()
                    .then_some(()))
            },
        )
        .await
        .expect("leftover Restore should delete");
    }
    let restore = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": "e2e-leak-restore", "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": "e2e-leak-repo" },
            "source": { "snapshotRef": { "name": "e2e-leak-restore-src" } },
            "target": { "pvcRef": { "name": "e2e-dst" } }
        }
    });
    restores
        .create(&PostParams::default(), &cr(restore))
        .await
        .expect("create Restore");
    wait_phase(&restores, "e2e-leak-restore", "Completed")
        .await
        .expect("Restore should reach Completed");

    // No per-run ConfigMap was created; the Job (default 1h TTL) persists.
    assert_no_work_spec_cm(&cms, "e2e-leak-restore", "after Completed").await;
    assert!(
        jobs.get_opt("e2e-leak-restore")
            .await
            .expect("get restore Job")
            .is_some(),
        "the completed restore Job must persist to its TTL"
    );
    assert_cm_stays_gone(
        &cms,
        "e2e-leak-restore",
        "waiting after a Completed Restore",
    )
    .await;
}

// ---------------------------------------------------------------------------
// 5. Orphan sweep: a work-spec ConfigMap with no Job is reaped; one with a
//    live Job survives.
// ---------------------------------------------------------------------------

/// The upgrade/backfill path: ConfigMaps left behind by pre-fix operator
/// versions (their Jobs long TTL-reaped) are invisible to the transition reap —
/// the periodic sweep must delete them, and ONLY them. The harness runs the
/// sweep fast (`KOPIUR_WORK_SPEC_SWEEP_*` in deploy/e2e/values.yaml).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn sweep_reaps_orphaned_work_spec_cm_but_not_a_live_runs() {
    let Some(world) = World::connect().await else {
        return;
    };
    let client = world.client().clone();
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let managed = serde_json::json!({ "app.kubernetes.io/managed-by": "kopiur" });

    // The orphan: kopiur-managed, work-spec-keyed, NO same-named Job — exactly
    // what a pre-fix operator left behind after the Job's TTL.
    let orphan = serde_json::json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": { "name": "e2e-sweep-orphan", "namespace": E2E_NAMESPACE, "labels": managed },
        "data": { "work-spec.json": "{}" }
    });
    let _ = cms.create(&PostParams::default(), &cr(orphan)).await;

    // The control: same shape but with a live same-named Job — a "running
    // mover" the sweep must never touch.
    let control_cm = serde_json::json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": { "name": "e2e-sweep-live", "namespace": E2E_NAMESPACE, "labels": managed },
        "data": { "work-spec.json": "{}" }
    });
    let _ = cms.create(&PostParams::default(), &cr(control_cm)).await;
    let control_job = serde_json::json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": { "name": "e2e-sweep-live", "namespace": E2E_NAMESPACE, "labels": managed },
        "spec": {
            "template": {
                "spec": {
                    "restartPolicy": "Never",
                    "containers": [{
                        "name": "sleep",
                        "image": consts::BUSYBOX_IMAGE,
                        "command": ["sh", "-c", "sleep 600"]
                    }]
                }
            }
        }
    });
    let _ = jobs.create(&PostParams::default(), &cr(control_job)).await;

    // The sweep (fast harness cadence) reaps the orphan once it ages past the
    // harness min-age.
    wait_until(
        "orphaned work-spec ConfigMap swept",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(cms
                .get_opt("e2e-sweep-orphan")
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("the sweep must delete an aged work-spec ConfigMap with no Job");

    // The control survived that same sweep pass (it was as old as the orphan).
    assert!(
        cms.get_opt("e2e-sweep-live")
            .await
            .expect("get control ConfigMap")
            .is_some(),
        "the sweep deleted a work-spec ConfigMap whose Job is still live"
    );

    // Cleanup.
    let _ = jobs
        .delete("e2e-sweep-live", &DeleteParams::background())
        .await;
    let _ = cms.delete("e2e-sweep-live", &DeleteParams::default()).await;
}
