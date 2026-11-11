//! e2e: `moverDefaults` inheritance, the bootstrap-gap fix, and the field-wise
//! security-context merge across all three layers — `moverDefaults` ⊂
//! `SnapshotPolicy.spec.mover` ⊂ `Snapshot.spec.mover` (ADR-0004 §1/§2, #464).
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use kube::Api;
use kube::api::{DeleteParams, PostParams};

use k8s_openapi::api::batch::v1::Job;
use kopiur_api::{Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World};

/// THE HEADLINE. A `Repository.spec.moverDefaults` security/pod context must reach
/// (1) the BOOTSTRAP Job's pod (the bootstrap-gap fix — before ADR-0004 the
/// connect/create Job ignored moverDefaults, so a filesystem repo on a
/// non-65532-owned dir was un-bootstrappable), and (2) the backup mover's pod. A
/// per-recipe `mover.securityContext.runAsUser` then merges OVER moverDefaults, a
/// PER-RUN `Snapshot.spec.mover.securityContext.runAsUser` merges over THAT (#464), and
/// in every rendered container the hardened `drop:[ALL]`/seccomp SURVIVES.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn mover_defaults_inherited_by_bootstrap_and_backup_with_recipe_override() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_repo(&client, "moverdefaults").await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-md-repo";
    // moverDefaults inherited by EVERY mover (bootstrap + backup): pod fsGroup +
    // container runAsUser/runAsGroup. The mover image runs as 65532, so keep the repo
    // dir accessible — these values just prove inheritance, not a UID the kopia run
    // must match for a hostPath repo (kopia creates the subdir as the running UID).
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                repo,
                "moverdefaults",
                serde_json::json!({
                    "moverDefaults": {
                        "podSecurityContext": { "fsGroup": 65532 },
                        "securityContext": { "runAsUser": 65532, "runAsGroup": 65532, "runAsNonRoot": true }
                    }
                }),
            )),
        )
        .await
        .expect("create Repository with moverDefaults");

    // (1) BOOTSTRAP GAP FIX: the connect/create Job (`<repo>-discovery`) inherits the
    //     repository's moverDefaults. Before ADR-0004 the bootstrap path used a bare
    //     hardened context and ignored moverDefaults entirely.
    let boot = wait_for_job(&jobs, &format!("{repo}-discovery")).await;
    assert_eq!(
        job_pod_sc(&boot).and_then(|sc| sc.fs_group),
        Some(65532),
        "bootstrap Job pod must inherit moverDefaults.podSecurityContext.fsGroup (the gap fix)"
    );
    let boot_sc = job_container_sc(&boot).expect("bootstrap container securityContext");
    assert_eq!(
        boot_sc.run_as_user,
        Some(65532),
        "bootstrap Job container must inherit moverDefaults.securityContext.runAsUser"
    );
    assert_hardening_survives(&boot_sc, "bootstrap");

    wait_phase(&repos, repo, "Ready").await.expect(
        "Repository should bootstrap to Ready (proving moverDefaults didn't break bootstrap)",
    );

    // (2) A backup mover whose recipe `mover.securityContext.runAsUser` OVERRIDES
    //     moverDefaults (3000 wins over 65532), while the pod fsGroup still inherits
    //     from moverDefaults (the recipe set no podSecurityContext) and the hardened
    //     drop:[ALL]/seccomp survive the field-wise merge.
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                "e2e-md-policy",
                "Repository",
                repo,
                serde_json::json!({
                    "mover": { "securityContext": { "runAsUser": 3000, "runAsNonRoot": true } }
                }),
            )),
        )
        .await
        .expect("create SnapshotPolicy with a recipe mover override");
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-md-backup",
                "e2e-md-policy",
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create Snapshot");

    let bjob = wait_for_job(&jobs, "e2e-md-backup").await;
    let bsc = job_container_sc(&bjob).expect("backup container securityContext");
    assert_eq!(
        bsc.run_as_user,
        Some(3000),
        "recipe mover.securityContext.runAsUser (3000) must win over moverDefaults (65532)"
    );
    assert_eq!(
        job_pod_sc(&bjob).and_then(|sc| sc.fs_group),
        Some(65532),
        "backup mover pod must inherit moverDefaults.podSecurityContext.fsGroup (recipe set none)"
    );
    assert_hardening_survives(&bsc, "backup (recipe override)");

    // (3) THE THIRD LAYER (#464): the SAME policy, with a PER-RUN
    //     `Snapshot.spec.mover.securityContext.runAsUser` (3001) merged OVER the
    //     recipe's 3000, which is itself over moverDefaults' 65532. This is what lets
    //     an ad-hoc one-shot pick its own mover identity without editing the shared,
    //     GitOps-managed SnapshotPolicy every scheduled run also uses. The pod fsGroup
    //     must STILL come from moverDefaults (neither the recipe nor the run sets one),
    //     proving the per-run layer merges field-wise rather than replacing the chain.
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-md-backup-run",
                "e2e-md-policy",
                // Bare spec fields, NOT a `{"spec": ...}` wrapper: a wrapper is dropped
                // by serde AND pruned by the apiserver, and the test would then silently
                // run against the recipe's 3000 and "pass".
                serde_json::json!({
                    "mover": { "securityContext": { "runAsUser": 3001, "runAsNonRoot": true } }
                }),
            )),
        )
        .await
        .expect("create Snapshot with a per-run mover override");

    // Read the CR BACK and assert the field actually landed. A CRD that never
    // regenerated would prune `spec.mover` server-side, and every assertion below
    // would then be satisfied by the recipe's own 3000 — a green test proving nothing.
    let stored = backups
        .get("e2e-md-backup-run")
        .await
        .expect("read the Snapshot back");
    assert_eq!(
        stored
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.security_context.as_ref())
            .and_then(|sc| sc.run_as_user),
        Some(3001),
        "spec.mover.securityContext.runAsUser must survive the apiserver round-trip —          if it is absent, the Snapshot CRD is stale and pruned the field"
    );

    let rjob = wait_for_job(&jobs, "e2e-md-backup-run").await;
    let rsc = job_container_sc(&rjob).expect("per-run backup container securityContext");
    assert_eq!(
        rsc.run_as_user,
        Some(3001),
        "Snapshot.spec.mover.securityContext.runAsUser (3001) must win over the          recipe's (3000) and moverDefaults' (65532)"
    );
    assert_eq!(
        job_pod_sc(&rjob).and_then(|sc| sc.fs_group),
        Some(65532),
        "the per-run mover pod must STILL inherit moverDefaults.podSecurityContext.fsGroup          (neither the recipe nor the run set one) — the per-run layer merges, it does not          replace the chain"
    );
    assert_hardening_survives(&rsc, "backup (per-run override)");

    // Cleanup.
    let _ = backups
        .delete("e2e-md-backup-run", &DeleteParams::default())
        .await;
    let _ = backups
        .delete("e2e-md-backup", &DeleteParams::default())
        .await;
    let _ = policies
        .delete("e2e-md-policy", &DeleteParams::default())
        .await;
    let _ = repos.delete(repo, &DeleteParams::default()).await;
}
