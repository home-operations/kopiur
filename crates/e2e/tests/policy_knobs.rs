//! e2e: SnapshotPolicy "knob" surfaces that previously had no end-to-end guard —
//! `errorHandling.ignoreFileErrors` (against a REAL unreadable file), the
//! `compression` / `upload` tuning (asserted at the controller→mover work-spec
//! contract, the seam where a regression would silently drop them while the
//! snapshot still succeeded), and the default `files.ignoreRules` OS-artifact
//! exclude set (task PR4 — proven against a REAL `lost+found` dir end to end).
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; skip gracefully off-cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;

use common::{
    cr, ensure_repo, observed_snapshot_count, repository_json, snapshot_json, snapshot_policy_json,
    wait_for_work_spec_json, wait_phase,
};
use kube::api::{DeleteParams, PostParams};
use kube::{Api, Client};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;

use kopiur_api::{Repository, Restore, Snapshot, SnapshotPolicy};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, builders, consts, wait};

const SUBPATH: &str = "errh";
const REPO: &str = "e2e-knobs-repo";

async fn ensure_knobs_repo(client: &kube::Client) {
    ensure_repo(client, SUBPATH).await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let _ = repos
        .create(
            &PostParams::default(),
            &cr(repository_json(REPO, SUBPATH, serde_json::json!({}))),
        )
        .await;
    wait_phase(&repos, REPO, "Ready")
        .await
        .expect("knobs repository should reach Ready");
}

/// `errorHandling.ignoreFileErrors` against a REAL unreadable file (root-owned,
/// mode 0000, in the node-seeded `src-eh` dir). The negative control — the same
/// source WITHOUT the flag must FAIL — proves the fixture actually breaks a
/// default backup, so the flagged run passing means the flag reached kopia.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn ignore_file_errors_lets_snapshot_complete() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::ErrorSource])
        .await
        .expect("fixtures ready");
    let client = world.client().clone();
    ensure_knobs_repo(&client).await;

    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // Negative control: defaults must FAIL on the unreadable file.
    let strict = snapshot_policy_json(
        E2E_NAMESPACE,
        "e2e-eh-strict-cfg",
        "Repository",
        REPO,
        serde_json::json!({ "sources": [ { "pvc": { "name": consts::PVC_SRC_EH } } ] }),
    );
    let _ = configs.create(&PostParams::default(), &cr(strict)).await;
    let _ = backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-eh-strict",
                "e2e-eh-strict-cfg",
                // One attempt — the failure is deterministic.
                serde_json::json!({ "failurePolicy": { "backoffLimit": 0 } }),
            )),
        )
        .await;
    wait_phase(&backups, "e2e-eh-strict", "Failed")
        .await
        .expect(
            "a DEFAULT backup of a source with an unreadable file must fail — if this \
             succeeded, the fixture no longer breaks a default run and the positive case \
             below proves nothing",
        );

    // With the flag: the same source backs up cleanly.
    let lenient = snapshot_policy_json(
        E2E_NAMESPACE,
        "e2e-eh-lenient-cfg",
        "Repository",
        REPO,
        serde_json::json!({
            "sources": [ { "pvc": { "name": consts::PVC_SRC_EH } } ],
            "errorHandling": { "ignoreFileErrors": true }
        }),
    );
    let _ = configs.create(&PostParams::default(), &cr(lenient)).await;
    let _ = backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-eh-lenient",
                "e2e-eh-lenient-cfg",
                serde_json::json!({}),
            )),
        )
        .await;
    wait_phase(&backups, "e2e-eh-lenient", "Succeeded")
        .await
        .expect("ignoreFileErrors: true must let the backup complete past the unreadable file");

    // kopia-side proof that a real snapshot landed.
    let count = observed_snapshot_count(&client, "e2e-eh-verify", SUBPATH).await;
    assert!(
        count >= 1,
        "the lenient backup must have produced a kopia snapshot; verifier saw {count}"
    );
}

/// `compression` + `upload` knobs reach the controller→mover work-spec (the
/// Job-embedded contract the mover's `kopia policy set` consumes — its flag
/// is unit-tested in `crates/kopia`), and kopia accepts them (`Succeeded`: a bad
/// compressor would fail the mover's policy-set step).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn compression_and_upload_knobs_reach_the_mover_contract() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("fixtures ready");
    let client = world.client().clone();
    ensure_knobs_repo(&client).await;

    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let cfg = snapshot_policy_json(
        E2E_NAMESPACE,
        "e2e-knobs-cfg",
        "Repository",
        REPO,
        serde_json::json!({
            "compression": { "compressor": "zstd", "neverCompress": ["*.zst"] },
            "upload": { "maxParallelSnapshots": 2, "maxParallelFileReads": 4 }
        }),
    );
    let _ = configs.create(&PostParams::default(), &cr(cfg)).await;
    let _ = backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-knobs",
                "e2e-knobs-cfg",
                serde_json::json!({}),
            )),
        )
        .await;

    // The mover Job's inline work-spec env carries the knobs (#224: the spec
    // rides the Job itself — no per-run ConfigMap). The Job outlives the run
    // by its TTL, so this read never races completion.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let spec = wait_for_work_spec_json(&jobs, "e2e-knobs").await;
    let policy = spec
        .pointer("/operation/snapshot/policy")
        .unwrap_or(&serde_json::Value::Null);
    assert_eq!(
        policy.get("compression").and_then(|v| v.as_str()),
        Some("zstd"),
        "compression must reach the mover contract; policy: {policy}"
    );
    assert_eq!(
        policy
            .get("neverCompress")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(1),
        "neverCompress must reach the mover contract; policy: {policy}"
    );
    assert_eq!(
        policy.get("maxParallelSnapshots").and_then(|v| v.as_i64()),
        Some(2),
        "upload.maxParallelSnapshots must reach the mover contract; policy: {policy}"
    );
    assert_eq!(
        policy.get("maxParallelFileReads").and_then(|v| v.as_i64()),
        Some(4),
        "upload.maxParallelFileReads must reach the mover contract; policy: {policy}"
    );

    // End-to-end close: kopia accepted the flags.
    wait_phase(&backups, "e2e-knobs", "Succeeded")
        .await
        .expect("the tuned backup should succeed (kopia accepted the policy flags)");
}

/// M4 flag sweep (issue #216 category sweep): `errorHandling.failFast` +
/// `upload.limitMb` (recipe, `SnapshotPolicy`) and `description`
/// (per-invocation, `Snapshot`) all reach the controller→mover work-spec
/// ConfigMap as `SnapshotOp` fields (`snapshot create` argv, NOT `policy set`
/// knobs — so they must NOT appear under `operation.snapshot.policy`), and
/// kopia accepts them (`Succeeded`).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn snapshot_create_knobs_reach_the_mover_contract() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("fixtures ready");
    let client = world.client().clone();
    ensure_knobs_repo(&client).await;

    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let cfg = snapshot_policy_json(
        E2E_NAMESPACE,
        "e2e-create-knobs-cfg",
        "Repository",
        REPO,
        serde_json::json!({
            "errorHandling": { "failFast": true },
            "upload": { "limitMb": 100 }
        }),
    );
    let _ = configs.create(&PostParams::default(), &cr(cfg)).await;
    let _ = backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-create-knobs",
                "e2e-create-knobs-cfg",
                serde_json::json!({ "description": "e2e m4 flag sweep" }),
            )),
        )
        .await;

    // The work-spec ConfigMap (same name as the mover Job) carries all three
    // knobs directly on `operation.snapshot` — NOT under `.policy` (the
    // `policy set` args), proving the recipe/invocation split holds end to end.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let spec = wait_for_work_spec_json(&jobs, "e2e-create-knobs").await;
    let snapshot_op = spec
        .pointer("/operation/snapshot")
        .unwrap_or(&serde_json::Value::Null);
    assert_eq!(
        snapshot_op.get("failFast").and_then(|v| v.as_bool()),
        Some(true),
        "errorHandling.failFast must reach SnapshotOp; op: {snapshot_op}"
    );
    assert_eq!(
        snapshot_op.get("uploadLimitMb").and_then(|v| v.as_i64()),
        Some(100),
        "upload.limitMb must reach SnapshotOp as uploadLimitMb; op: {snapshot_op}"
    );
    assert_eq!(
        snapshot_op.get("description").and_then(|v| v.as_str()),
        Some("e2e m4 flag sweep"),
        "Snapshot.spec.description must reach SnapshotOp; op: {snapshot_op}"
    );
    // Recipe knobs must NOT leak into the `policy set` args.
    let policy = snapshot_op
        .get("policy")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    assert!(
        policy.get("failFast").is_none(),
        "failFast must not leak into policy set args; policy: {policy}"
    );
    assert!(
        policy.get("limitMb").is_none(),
        "limitMb must not leak into policy set args; policy: {policy}"
    );

    // End-to-end close: kopia accepted the flags.
    wait_phase(&backups, "e2e-create-knobs", "Succeeded")
        .await
        .expect("the tuned backup should succeed (kopia accepted --fail-fast/--upload-limit-mb/--description)");
}

// --- task PR4: default `files.ignoreRules` OS-artifact excludes ---

/// Seed the test's OWN dynamic source PVC (never the shared `e2e-src` — content
/// written there would contaminate other shards' assertions, mirrors
/// `hooks.rs`'s `ensure_hooks_world`) with a `lost+found/` dir (the default
/// exclude the test proves) plus a `keep.txt` control file (proves the backup
/// isn't just empty/failed).
async fn ensure_ignore_rules_source(client: &Client, src_pvc: &str) {
    use kopiur_e2e::apply::{Fixture, apply_all};
    let fixtures: Vec<Fixture> = vec![builders::dynamic_pvc(E2E_NAMESPACE, src_pvc, "1Gi").into()];
    apply_all(client, &fixtures).await.expect("source PVC");

    let seeder = format!("{src_pvc}-seed");
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    if pods.get_opt(&seeder).await.ok().flatten().is_none() {
        let pod = builders::one_shot_pod(
            E2E_NAMESPACE,
            &seeder,
            &[
                "sh",
                "-c",
                "mkdir -p /data/lost+found && echo leftover > /data/lost+found/leftover.txt \
                 && echo keep > /data/keep.txt",
            ],
            &[(src_pvc, "/data")],
        );
        let _ = pods.create(&PostParams::default(), &pod).await;
    }
    wait::pod_succeeded(client, E2E_NAMESPACE, &seeder)
        .await
        .expect("ignore-rules source seeder pod should succeed");
}

/// Restore `snapshot` into a fresh PVC and assert, via a reader pod, that
/// `keep.txt` (the control file) IS present and `lost+found` (the default
/// exclude) is ABSENT — proving the default `files.ignoreRules` set actually
/// excludes at the kopia layer, end to end.
async fn assert_lost_and_found_excluded(client: &Client, snapshot: &str) {
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let name = format!("{snapshot}-verify");
    let dst = format!("{snapshot}-dst");
    let restore = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": REPO },
            "source": { "snapshotRef": { "name": snapshot } },
            "target": { "pvc": { "name": dst, "capacity": "1Gi" } }
        }
    });
    let _ = restores.create(&PostParams::default(), &cr(restore)).await;
    wait_phase(&restores, &name, "Completed")
        .await
        .expect("verification restore should complete");

    let reader = format!("{snapshot}-reader");
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    if pods.get_opt(&reader).await.ok().flatten().is_none() {
        let script = "test -f /restore/keep.txt && test ! -e /restore/lost+found";
        let pod = builders::one_shot_pod(
            E2E_NAMESPACE,
            &reader,
            &["sh", "-c", script],
            &[(dst.as_str(), "/restore")],
        );
        let _ = pods.create(&PostParams::default(), &pod).await;
    }
    wait::pod_succeeded(client, E2E_NAMESPACE, &reader)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "keep.txt must survive the default backup AND lost+found must be excluded by \
                 the default ignoreRules set: {e}"
            )
        });
    let _ = restores.delete(&name, &DeleteParams::default()).await;
    let _ = pods.delete(&reader, &DeleteParams::default()).await;
}

/// The load-bearing end-to-end proof for task PR4: a `SnapshotPolicy` with NO
/// `files` block at all (the common case — most policies never set it) must
/// still exclude the default OS-artifact set at the kopia layer. A negative
/// control proves the fixture is real: WITHOUT the default (explicit
/// `ignoreRules: []`, full opt-out) the same `lost+found` dir survives the
/// round trip, so the positive case passing means the default actually did the
/// excluding, not that the fixture never had anything to exclude.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test)"]
async fn default_ignore_rules_exclude_lost_and_found_end_to_end() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("fixtures ready");
    let client = world.client().clone();
    ensure_knobs_repo(&client).await;

    let configs: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // Positive case: absent `files:` — the default set must exclude lost+found.
    let src_default = "e2e-ignorerules-src-default";
    ensure_ignore_rules_source(&client, src_default).await;
    let cfg_default = snapshot_policy_json(
        E2E_NAMESPACE,
        "e2e-ignorerules-default-cfg",
        "Repository",
        REPO,
        serde_json::json!({ "sources": [ { "pvc": { "name": src_default } } ] }),
    );
    let _ = configs
        .create(&PostParams::default(), &cr(cfg_default))
        .await;
    let _ = backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-ignorerules-default",
                "e2e-ignorerules-default-cfg",
                serde_json::json!({}),
            )),
        )
        .await;
    wait_phase(&backups, "e2e-ignorerules-default", "Succeeded")
        .await
        .expect("default backup (no files: block) should succeed");
    assert_lost_and_found_excluded(&client, "e2e-ignorerules-default").await;

    // Negative control: explicit `ignoreRules: []` (full opt-out) — the same
    // source's lost+found dir must NOT be excluded, proving the positive case
    // above genuinely exercised the default rather than an inert fixture.
    let src_optout = "e2e-ignorerules-src-optout";
    ensure_ignore_rules_source(&client, src_optout).await;
    let cfg_optout = snapshot_policy_json(
        E2E_NAMESPACE,
        "e2e-ignorerules-optout-cfg",
        "Repository",
        REPO,
        serde_json::json!({
            "sources": [ { "pvc": { "name": src_optout } } ],
            "files": { "ignoreRules": [] }
        }),
    );
    let _ = configs
        .create(&PostParams::default(), &cr(cfg_optout))
        .await;
    let _ = backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-ignorerules-optout",
                "e2e-ignorerules-optout-cfg",
                serde_json::json!({}),
            )),
        )
        .await;
    wait_phase(&backups, "e2e-ignorerules-optout", "Succeeded")
        .await
        .expect("opt-out backup (ignoreRules: []) should succeed");

    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let name = "e2e-ignorerules-optout-verify";
    let dst = "e2e-ignorerules-optout-dst";
    let restore = serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Restore",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": REPO },
            "source": { "snapshotRef": { "name": "e2e-ignorerules-optout" } },
            "target": { "pvc": { "name": dst, "capacity": "1Gi" } }
        }
    });
    let _ = restores.create(&PostParams::default(), &cr(restore)).await;
    wait_phase(&restores, name, "Completed")
        .await
        .expect("opt-out verification restore should complete");
    let reader = "e2e-ignorerules-optout-reader";
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    if pods.get_opt(reader).await.ok().flatten().is_none() {
        let pod = builders::one_shot_pod(
            E2E_NAMESPACE,
            reader,
            &["sh", "-c", "test -e /restore/lost+found"],
            &[(dst, "/restore")],
        );
        let _ = pods.create(&PostParams::default(), &pod).await;
    }
    wait::pod_succeeded(&client, E2E_NAMESPACE, reader)
        .await
        .expect(
            "with ignoreRules: [] (opt-out), lost+found must SURVIVE the round trip — if this \
             failed, the fixture no longer proves the positive case above meant anything",
        );
}
