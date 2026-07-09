//! e2e: first-class backup verification (ADR-0005 §4).
//!
//! - quick: `verification.quick` + `successExpr` drives a `kopia snapshot verify`
//!   mover Job and stamps `status.lastVerified`.
//! - deep: `verification.deep` drives a scratch-restore into `/scratch`. This is
//!   the regression guard for the production bug where the controller mounted
//!   nothing at `/scratch`, so the non-root mover's restore died with
//!   `mkdir /scratch: permission denied`. With `capacity`/`storageClassName`
//!   unset the scratch volume is an `emptyDir`; the run must succeed and stamp
//!   `status.lastVerified`.
//! - gate (GitHub #168): a brand-new policy with `verification` configured but
//!   NO backup yet must not spawn a verify Job (the mover would fail hard
//!   against an empty repository); the first verify catches up promptly once
//!   the first backup succeeds.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use std::time::Duration;

use kube::Api;
use kube::api::{ListParams, Patch, PatchParams, PostParams};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ConfigMap;
use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

/// Wait for a verify Job matching `selector`, then return its inline work-spec
/// env (`KOPIUR_WORK_SPEC`), parsed. Verify Job names are per-slot
/// (`<policy>-vfy-<q|d>-<unix_slot>`, [`crate::verify_job_name`] in the
/// controller), not deterministic like other mover Jobs, so the spec is read
/// from the FOUND Job. The spec rides the Job itself (#224 — no per-run
/// ConfigMap), and the Job outlives the slot by its TTL, so this never races
/// completion.
async fn verify_work_spec_json(client: &kube::Client, selector: &str) -> serde_json::Value {
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let job = wait_until(
        "verify mover Job created",
        default_timeout(),
        poll_interval(),
        || async {
            let lp = ListParams::default().labels(selector);
            Ok(jobs.list(&lp).await?.items.into_iter().next())
        },
    )
    .await
    .expect("a verify Job should be spawned");
    let raw = job
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|p| p.containers.first())
        .and_then(|c| c.env.as_ref())
        .and_then(|env| env.iter().find(|e| e.name == "KOPIUR_WORK_SPEC"))
        .and_then(|e| e.value.clone())
        .expect("verify Job carries the inline work-spec env");
    serde_json::from_str(&raw).expect("work-spec env parses as JSON")
}

/// Verification (ADR-0005 §4): a `SnapshotPolicy.spec.verification.quick` with an
/// every-minute cron and a `successExpr` over the result drives a `kopia snapshot
/// verify` mover Job; on success the controller stamps `status.lastVerified`.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn verification_quick_with_success_expr_stamps_last_verified() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    // Seed a real snapshot so quick-verify has something to verify.
    ensure_seed(
        &client,
        "e2e-verify-repo",
        "e2e-verify-policy",
        "e2e-verify-seed",
        "verify",
    )
    .await;

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    // Patch verification onto the existing policy: every-minute quick verify, gated by
    // a successExpr asserting the verify reported zero errors. Also sets the M3
    // (issue #216 category sweep) tuning knobs — parallel/fileParallelism/
    // fileQueueLength/maxErrors — the regression guard for the dormant-plumbing bug
    // where the controller hardcoded `max_errors: None, parallel: None` regardless
    // of what the CRD carried.
    let patch = serde_json::json!({
        "spec": { "verification": {
            "quick": {
                "schedule": { "cron": "* * * * *" },
                "parallel": 2,
                "fileParallelism": 4,
                "fileQueueLength": 100,
                "maxErrors": 1
            },
            "successExpr": "stats.errors == 0"
        } }
    });
    policies
        .patch(
            "e2e-verify-policy",
            &PatchParams::default(),
            &Patch::Merge(&patch),
        )
        .await
        .expect("patch verification onto the SnapshotPolicy");

    // Within a couple of minutes a quick-verify Job runs and stamps lastVerified.
    wait_until(
        "SnapshotPolicy.status.lastVerified is stamped by a passing quick verify",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&policies, "e2e-verify-policy").await;
            Ok(s.get("lastVerified")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|_| ()))
        },
    )
    .await
    .expect(
        "a passing quick verify (successExpr stats.errors == 0) must stamp status.lastVerified",
    );

    // The work-spec ConfigMap the mover actually ran against carries the tuning
    // knobs — not just that the run succeeded, but that the knobs reached the
    // mover contract (`operation.verify.tier.quick.*`).
    let selector = "app.kubernetes.io/component=verify,\
                    kopiur.home-operations.com/verify=e2e-verify-policy";
    let spec = verify_work_spec_json(&client, selector).await;
    let quick = spec
        .pointer("/operation/verify/tier/quick")
        .unwrap_or(&serde_json::Value::Null);
    assert_eq!(
        quick.get("parallel").and_then(|v| v.as_i64()),
        Some(2),
        "verification.quick.parallel must reach the mover contract; quick: {quick}"
    );
    assert_eq!(
        quick.get("fileParallelism").and_then(|v| v.as_i64()),
        Some(4),
        "verification.quick.fileParallelism must reach the mover contract; quick: {quick}"
    );
    assert_eq!(
        quick.get("fileQueueLength").and_then(|v| v.as_i64()),
        Some(100),
        "verification.quick.fileQueueLength must reach the mover contract; quick: {quick}"
    );
    assert_eq!(
        quick.get("maxErrors").and_then(|v| v.as_i64()),
        Some(1),
        "verification.quick.maxErrors must reach the mover contract; quick: {quick}"
    );

    // Leak guard (the "605 ConfigMaps" fix, #224): a verify slot creates NO
    // per-run ConfigMap at all — the spec rides the Job env. Per-slot names
    // used to accumulate one ConfigMap per slot on the long-lived
    // SnapshotPolicy, forever.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let lp = ListParams::default().labels(
        "app.kubernetes.io/component=verify,\
         kopiur.home-operations.com/verify=e2e-verify-policy",
    );
    for job in jobs.list(&lp).await.expect("list verify Jobs").items {
        let Some(job_name) = job.metadata.name else {
            continue;
        };
        assert!(
            cms.get_opt(&job_name)
                .await
                .expect("query ConfigMap")
                .is_none(),
            "verify slot {job_name} must not create a per-run work-spec ConfigMap (leak guard)"
        );
    }
}

/// Deep verification (ADR-0005 §4): a `SnapshotPolicy.spec.verification.deep` drives
/// a scratch-restore of the latest snapshot into `/scratch`. With `capacity`/
/// `storageClassName` unset the controller mounts a writable `emptyDir` there.
///
/// Regression guard for the production bug (`mkdir /scratch: permission denied`):
/// before the fix the controller mounted nothing at `/scratch`, so the non-root
/// mover could not create the restore target and every deep-verify Job failed.
/// The authoritative end-to-end proof is `status.lastVerified` being stamped — it
/// is only stamped after the scratch-restore succeeds and the (deep-only) CEL
/// `successExpr` over `restored` passes.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn verification_deep_scratch_restore_stamps_last_verified() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    // Seed a real snapshot so deep-verify has something to restore. Distinct
    // names from the quick test so the two can coexist in one cluster.
    ensure_seed(
        &client,
        "e2e-vfy-deep-repo",
        "e2e-vfy-deep-policy",
        "e2e-vfy-deep-seed",
        "vfydeep",
    )
    .await;

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    // Every-minute deep verify, capacity/storageClassName UNSET -> emptyDir scratch
    // (the path that regressed). The successExpr exercises the deep-only `restored`
    // environment, so a stamped lastVerified proves the scratch-restore ran. Also
    // sets `parallel` (M3 / issue #216 category sweep): deep verify IS a restore
    // under the hood, so this maps to `restore --parallel` in the mover.
    let patch = serde_json::json!({
        "spec": { "verification": {
            "deep": { "schedule": { "cron": "* * * * *" }, "parallel": 2 },
            "successExpr": "restored.files >= 0 && restored.checksumMatches"
        } }
    });
    policies
        .patch(
            "e2e-vfy-deep-policy",
            &PatchParams::default(),
            &Patch::Merge(&patch),
        )
        .await
        .expect("patch deep verification onto the SnapshotPolicy");

    wait_until(
        "SnapshotPolicy.status.lastVerified is stamped by a passing deep scratch-restore",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&policies, "e2e-vfy-deep-policy").await;
            Ok(s.get("lastVerified")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|_| ()))
        },
    )
    .await
    .expect(
        "a passing deep verify must mount a writable /scratch (emptyDir) and stamp \
         status.lastVerified — failure here is the `mkdir /scratch: permission denied` regression",
    );

    // The work-spec ConfigMap carries `parallel` through to the mover contract
    // (`operation.verify.tier.deep.parallel`).
    let selector = "app.kubernetes.io/component=verify,\
                    kopiur.home-operations.com/verify=e2e-vfy-deep-policy";
    let spec = verify_work_spec_json(&client, selector).await;
    let deep = spec
        .pointer("/operation/verify/tier/deep")
        .unwrap_or(&serde_json::Value::Null);
    assert_eq!(
        deep.get("parallel").and_then(|v| v.as_i64()),
        Some(2),
        "verification.deep.parallel must reach the mover contract; deep: {deep}"
    );
}

/// The deep-verify mover inherits the repository's `moverDefaults` — set both the
/// scratch (`moverDefaults.scratch`) and the kopia cache (`moverDefaults.cache`)
/// size/class ONCE on the `Repository`, leave `verification.deep` bare, and the
/// spawned deep-verify Job's `scratch` AND `kopia-cache` volumes must both be sized
/// ephemeral PVCs carrying the inherited `storageClassName` + `capacity`.
///
/// Regression guard for the inheritance wiring (`moverDefaults.scratch ⊂
/// verification.deep`, and `moverDefaults.cache` → verify cache volume). Asserted at
/// the **Job spec** level (no PVC provisioning required), so synthetic StorageClasses
/// are fine and the test stays fast — we only enable `deep` (no `quick`) so the single
/// verify Job is the deep one.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn deep_verify_scratch_inherits_repo_mover_defaults() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_seed(
        &client,
        "e2e-vfy-inh-repo",
        "e2e-vfy-inh-policy",
        "e2e-vfy-inh-seed",
        "vfyinh",
    )
    .await;

    // Scratch + cache defaults set ONCE on the Repository (the user's "configure it
    // centrally" ask). Synthetic classes are fine — we assert the Job spec, not bound
    // PVCs. cache.mode: Persistent must be COERCED to a per-run ephemeral volume for
    // verify (it must never attach the backup's warm RWO PVC).
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let repo_patch = serde_json::json!({
        "spec": { "moverDefaults": {
            "scratch": {
                "capacity": "1Gi",
                "storageClassName": "e2e-scratch-class"
            },
            "cache": {
                "capacity": "2Gi",
                "storageClassName": "e2e-cache-class",
                "mode": "Persistent"
            }
        } }
    });
    repos
        .patch(
            "e2e-vfy-inh-repo",
            &PatchParams::default(),
            &Patch::Merge(&repo_patch),
        )
        .await
        .expect("patch moverDefaults.scratch + moverDefaults.cache onto the Repository");

    // Enable ONLY deep verify, with a bare schedule — scratch size/class come entirely
    // from the inherited repo default.
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policy_patch = serde_json::json!({
        "spec": { "verification": { "deep": { "schedule": { "cron": "* * * * *" } } } }
    });
    policies
        .patch(
            "e2e-vfy-inh-policy",
            &PatchParams::default(),
            &Patch::Merge(&policy_patch),
        )
        .await
        .expect("patch verification.deep onto the SnapshotPolicy");

    // The only verify Job is the deep one (no quick configured). Wait for it to appear.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let selector = "app.kubernetes.io/component=verify,\
                    kopiur.home-operations.com/verify=e2e-vfy-inh-policy";
    let job = wait_until(
        "deep-verify mover Job created",
        default_timeout(),
        poll_interval(),
        || async {
            let lp = ListParams::default().labels(selector);
            Ok(jobs.list(&lp).await?.items.into_iter().next())
        },
    )
    .await
    .expect("a deep-verify Job should be spawned");

    // The inherited capacities make BOTH the scratch and the kopia cache sized
    // ephemeral PVCs (not emptyDirs), carrying the repo-default class + capacity.
    let scratch = job_scratch_volume(&job).expect("deep-verify Job must mount a 'scratch' volume");
    let (scratch_class, scratch_cap) = ephemeral_class_and_capacity(&scratch).expect(
        "inherited scratch capacity must make scratch a sized ephemeral PVC, not an emptyDir",
    );
    assert_eq!(
        scratch_class.as_deref(),
        Some("e2e-scratch-class"),
        "scratch PVC must inherit moverDefaults.scratch.storageClassName"
    );
    assert_eq!(
        scratch_cap.as_deref(),
        Some("1Gi"),
        "scratch PVC must inherit moverDefaults.scratch.capacity"
    );

    let cache = job_cache_volume(&job).expect("deep-verify Job must mount a 'kopia-cache' volume");
    let (cache_class, cache_cap) = ephemeral_class_and_capacity(&cache).expect(
        "inherited cache capacity must make the verify cache a sized EPHEMERAL PVC \
         (a Persistent moverDefaults.cache is coerced to ephemeral for verify), not an emptyDir",
    );
    assert_eq!(
        cache_class.as_deref(),
        Some("e2e-cache-class"),
        "verify cache PVC must inherit moverDefaults.cache.storageClassName"
    );
    assert_eq!(
        cache_cap.as_deref(),
        Some("2Gi"),
        "verify cache PVC must inherit moverDefaults.cache.capacity"
    );
}

/// GitHub #168 regression: a brand-new `SnapshotPolicy` with BOTH verification
/// tiers configured — but no backup yet — must NOT spawn a verify Job. Before the
/// fix, `due_tier`'s catch-up logic anchored a never-verified policy a year in the
/// past, so the very first reconcile treated every configured tier as already
/// past-due and spawned a verify Job against a repository with zero snapshots;
/// the mover failed hard (`deep verify found no snapshot to restore …`).
/// Verification unlocks on this policy's first successful backup (or, for an
/// adopted repository, discovered snapshots already present — not exercised
/// here), at which point the catch-up fires promptly (asserted below).
///
/// Deliberately does NOT call `ensure_seed` (every other test in this file does)
/// — that is exactly the scenario #168 needs: `verification` configured before
/// any snapshot exists.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn verification_gated_until_first_successful_snapshot_168() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    // A Ready Repository + SnapshotPolicy with NO Snapshot at all (mirrors
    // ensure_seed's fixtures minus the seed leg) — the #168 precondition.
    ensure_empty_policy(
        &client,
        "e2e-vfy-gate-repo",
        "e2e-vfy-gate-policy",
        "vfygate",
    )
    .await;

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    // Both tiers, every minute, new nested shape: gives pre-fix code the best
    // chance to fire a verify Job well within the assertion window below.
    let patch = serde_json::json!({
        "spec": { "verification": {
            "quick": { "schedule": { "cron": "* * * * *" } },
            "deep": { "schedule": { "cron": "* * * * *" } },
            "successExpr": "stats.errors == 0"
        } }
    });
    policies
        .patch(
            "e2e-vfy-gate-policy",
            &PatchParams::default(),
            &Patch::Merge(&patch),
        )
        .await
        .expect("patch verification onto the SnapshotPolicy");

    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let selector = "app.kubernetes.io/component=verify,\
                    kopiur.home-operations.com/verify=e2e-vfy-gate-policy";

    // --- Gated: no verify Job for a fixed window, and no lastVerified/Failed
    // verify reported. 45s comfortably exceeds both the every-minute cron cadence
    // and the controller's reconcile-on-create — on pre-fix code the past-due
    // catch-up slot is consumed on the very first reconcile (seconds, not
    // minutes), so a real regression is unambiguous well inside this window
    // (same margin as the `QUIET_WINDOW` precedent in steady_state.rs).
    tokio::time::sleep(Duration::from_secs(45)).await;
    let found = jobs
        .list(&ListParams::default().labels(selector))
        .await
        .expect("list verify Jobs")
        .items;
    assert!(
        found.is_empty(),
        "no verify Job may exist before any snapshot succeeds (#168), found: {:?}",
        found
            .iter()
            .map(|j| j.metadata.name.clone())
            .collect::<Vec<_>>()
    );
    let gated_status = status_json(&policies, "e2e-vfy-gate-policy").await;
    assert!(
        gated_status
            .get("lastVerified")
            .and_then(|v| v.as_str())
            .is_none(),
        "status.lastVerified must stay unset while gated, got: {gated_status:?}"
    );

    // --- Unlock the gate: create/seed the first backup, the same mechanism
    // `ensure_seed`'s snapshot leg uses, and wait for it to Succeed.
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-vfy-gate-seed",
                "e2e-vfy-gate-policy",
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create the first Snapshot");
    wait_phase(&backups, "e2e-vfy-gate-seed", "Succeeded")
        .await
        .expect("the first backup should succeed");

    // --- Catch-up: now unlocked, the first due verify fires promptly and stamps
    // lastVerified (reuses the assertion pattern of the quick-verify test above).
    wait_until(
        "SnapshotPolicy.status.lastVerified is stamped after the first successful backup (#168 catch-up)",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&policies, "e2e-vfy-gate-policy").await;
            Ok(s.get("lastVerified")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|_| ()))
        },
    )
    .await
    .expect(
        "verification must catch up promptly once the first successful backup unlocks the gate",
    );

    let after_unlock = jobs
        .list(&ListParams::default().labels(selector))
        .await
        .expect("list verify Jobs")
        .items;
    assert!(
        !after_unlock.is_empty(),
        "a verify Job must have fired after the first successful backup unlocked verification"
    );
}
