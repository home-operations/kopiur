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
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use kube::Api;
use kube::api::{ListParams, Patch, PatchParams};

use k8s_openapi::api::batch::v1::Job;
use kopiur_api::{Repository, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

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
    // a successExpr asserting the verify reported zero errors.
    let patch = serde_json::json!({
        "spec": { "verification": {
            "quick": { "cron": "* * * * *" },
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
    // environment, so a stamped lastVerified proves the scratch-restore ran.
    let patch = serde_json::json!({
        "spec": { "verification": {
            "deep": { "schedule": { "cron": "* * * * *" } },
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
}

/// Repo-level `moverDefaults.scratch` is inherited by a policy's `verification.deep`
/// scratch PVC — set the scratch size/class ONCE on the `Repository`, leave
/// `verification.deep` bare, and the spawned deep-verify Job's scratch volume must be
/// a sized ephemeral PVC carrying the inherited `storageClassName` + `capacity`.
///
/// Regression guard for the inheritance wiring (`moverDefaults.scratch ⊂
/// verification.deep`). Asserted at the **Job spec** level (no PVC provisioning
/// required), so a synthetic StorageClass is fine and the test stays fast — we only
/// enable `deep` (no `quick`) so the single verify Job is the deep one.
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

    // Scratch defaults set ONCE on the Repository (the user's "configure it centrally"
    // ask). A synthetic class is fine — we assert the Job spec, not a bound PVC.
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let repo_patch = serde_json::json!({
        "spec": { "moverDefaults": { "scratch": {
            "capacity": "1Gi",
            "storageClassName": "e2e-scratch-class"
        } } }
    });
    repos
        .patch(
            "e2e-vfy-inh-repo",
            &PatchParams::default(),
            &Patch::Merge(&repo_patch),
        )
        .await
        .expect("patch moverDefaults.scratch onto the Repository");

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

    // The inherited capacity makes scratch a SIZED ephemeral PVC (not an emptyDir),
    // and the storageClass/capacity are the repo defaults.
    let scratch = job_scratch_volume(&job).expect("deep-verify Job must mount a 'scratch' volume");
    let tmpl = scratch
        .ephemeral
        .as_ref()
        .and_then(|e| e.volume_claim_template.as_ref())
        .expect(
            "inherited scratch capacity must make scratch a sized ephemeral PVC, not an emptyDir",
        );
    assert_eq!(
        tmpl.spec.storage_class_name.as_deref(),
        Some("e2e-scratch-class"),
        "scratch PVC must inherit moverDefaults.scratch.storageClassName"
    );
    let storage = tmpl
        .spec
        .resources
        .as_ref()
        .and_then(|r| r.requests.as_ref())
        .and_then(|m| m.get("storage"))
        .map(|q| q.0.clone());
    assert_eq!(
        storage.as_deref(),
        Some("1Gi"),
        "scratch PVC must inherit moverDefaults.scratch.capacity"
    );
}
