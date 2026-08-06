//! #351 — a kopia-deduped backup must be `Unchanged`, not `Failed`.
//!
//! The reported symptom was a `Snapshot` failing terminally with
//! `no JSON output found on stdout for 'snapshot create result'` whenever kopia
//! declined to write a manifest because nothing had changed. That is a whole-
//! pipeline bug — controller → work spec → mover → kopia → status — so it gets a
//! whole-pipeline test rather than only unit coverage of the seam.
//!
//! Needs no CSI: `copyMethod: Direct` over the filesystem fixture is exactly the
//! shape that reproduces it.

#![cfg(all(unix, feature = "e2e"))]

mod common;

use common::*;
use kopiur_e2e::{E2E_NAMESPACE, Need, World};
use kube::Api;
use kube::api::PostParams;

/// Two back-to-back runs of an opt-in policy over static data.
///
/// Before the fix the SECOND one failed terminally. It must now be `Unchanged`:
/// a success that owns no kopia manifest.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:up)"]
async fn a_second_run_over_unchanged_data_is_unchanged_not_failed() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_repo(&client, "unchanged-dedup").await;

    let repos: Api<kopiur_api::Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<kopiur_api::SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<kopiur_api::Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-repo-unchanged-dedup";
    let _ = repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                repo,
                "unchanged-dedup",
                serde_json::json!({}),
            )),
        )
        .await;
    wait_phase(&repos, repo, "Ready").await.expect("repo Ready");

    let policy = "e2e-unchanged";
    let _ = policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                policy,
                "Repository",
                "e2e-repo-unchanged-dedup",
                serde_json::json!({
                    // The opt-in. Without it the mover pins
                    // `--ignore-identical-snapshots=false` at the identity
                    // scope and kopia always writes a manifest — which the
                    // sibling test below proves.
                    "files": { "ignoreIdenticalSnapshots": true },
                    "identity": { "username": "unchanged", "hostname": "e2e" }
                }),
            )),
        )
        .await;

    // The opt-in is the ENTIRE premise of this test: with it absent, kopia
    // writes a manifest every time and the assertion below fails as "the
    // feature does not work" while the real fault is that the field never
    // reached the CR. Prove it landed before spending seven minutes waiting.
    let created = policies.get(policy).await.expect("read back the policy");
    assert!(
        created
            .spec
            .files
            .as_ref()
            .is_some_and(|f| f.ignore_identical_snapshots),
        "files.ignoreIdenticalSnapshots must be set on the created policy, got {:?}",
        created.spec.files
    );

    // First run: a real snapshot.
    let first = "e2e-unchanged-1";
    create_idempotent(
        &backups,
        &cr(snapshot_json(
            E2E_NAMESPACE,
            first,
            policy,
            serde_json::json!({}),
        )),
        "create first Snapshot",
    )
    .await;
    wait_phase(&backups, first, "Succeeded")
        .await
        .expect("the first run creates a snapshot");
    let first_status = status_json(&backups, first).await;
    let first_id = first_status["snapshot"]["kopiaSnapshotID"]
        .as_str()
        .expect("the first run owns a kopia snapshot")
        .to_string();

    // Second run over byte-identical data: kopia dedupes it away.
    let second = "e2e-unchanged-2";
    create_idempotent(
        &backups,
        &cr(snapshot_json(
            E2E_NAMESPACE,
            second,
            policy,
            serde_json::json!({}),
        )),
        "create second Snapshot",
    )
    .await;
    wait_phase(&backups, second, "Unchanged")
        .await
        .expect("a deduped run is Unchanged — this is the #351 regression");

    let s = status_json(&backups, second).await;
    // The load-bearing assertion. Reporting the previous run's id here is the
    // ownership corruption the fix exists to prevent: two CRs claiming one
    // manifest, and the first prune deleting it out from under the second.
    assert!(
        s["snapshot"].is_null(),
        "an Unchanged run must claim NO kopia snapshot, got {:?}",
        s["snapshot"]
    );
    assert!(
        s.get("failure").is_none_or(serde_json::Value::is_null),
        "a dedupe is not a failure: {:?}",
        s["failure"]
    );
    // Timing IS recorded, or the policy's last-backup timestamp would freeze
    // and KopiurBackupStale would page for a source that simply is not changing.
    assert!(
        s["timing"]["endTime"].is_string(),
        "timing must be recorded so liveness advances: {:?}",
        s["timing"]
    );
    // Ready, not Stalled: the source IS protected, by the previous snapshot.
    wait_condition(&backups, second, "Ready", "True")
        .await
        .expect("Unchanged is Ready");

    // And the first run's manifest is untouched.
    let again = status_json(&backups, first).await;
    assert_eq!(
        again["snapshot"]["kopiaSnapshotID"].as_str(),
        Some(first_id.as_str()),
        "the previous run's manifest must not be disturbed"
    );

    let _ = backups.delete(first, &Default::default()).await;
    let _ = backups.delete(second, &Default::default()).await;
    let _ = policies.delete(policy, &Default::default()).await;
}

/// The default path: with the knob OFF, kopia must write a manifest every time.
///
/// This is the half that makes the reported bug unreachable by default. The
/// mover pins `--ignore-identical-snapshots=false` at the kopia identity scope
/// on every run, so even a repository whose GLOBAL kopia policy enables dedupe
/// cannot silently turn it on — which is how #351 was reachable without the CRD
/// field ever having been wired.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:up)"]
async fn without_the_opt_in_every_run_writes_a_distinct_manifest() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_repo(&client, "unchanged-default").await;

    let repos: Api<kopiur_api::Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<kopiur_api::SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<kopiur_api::Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    let repo = "e2e-repo-unchanged-default";
    let _ = repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                repo,
                "unchanged-default",
                serde_json::json!({}),
            )),
        )
        .await;
    wait_phase(&repos, repo, "Ready").await.expect("repo Ready");

    let policy = "e2e-unchanged-off";
    let _ = policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                policy,
                "Repository",
                "e2e-repo-unchanged-default",
                serde_json::json!({
                    "identity": { "username": "unchangedoff", "hostname": "e2e" }
                }),
            )),
        )
        .await;

    let mut ids = Vec::new();
    for n in 1..=2 {
        let name = format!("e2e-unchanged-off-{n}");
        create_idempotent(
            &backups,
            &cr(snapshot_json(
                E2E_NAMESPACE,
                &name,
                policy,
                serde_json::json!({}),
            )),
            "create Snapshot",
        )
        .await;
        wait_phase(&backups, &name, "Succeeded")
            .await
            .expect("the default path always writes a manifest");
        let s = status_json(&backups, &name).await;
        ids.push(
            s["snapshot"]["kopiaSnapshotID"]
                .as_str()
                .expect("owns a kopia snapshot")
                .to_string(),
        );
    }
    assert_ne!(
        ids[0], ids[1],
        "with the knob off each run must own its OWN manifest — a shared id is the \
         two-CRs-one-manifest corruption"
    );

    for n in 1..=2 {
        let _ = backups
            .delete(&format!("e2e-unchanged-off-{n}"), &Default::default())
            .await;
    }
    let _ = policies.delete(policy, &Default::default()).await;
}
