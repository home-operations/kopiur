//! e2e: `SnapshotReplication` (issue #368) — **logical** (snapshot-level)
//! replication between two real repository CRs via `kopia snapshot migrate`.
//!
//! Where `RepositoryReplication` (tests/replication.rs) mirrors *blobs* to an
//! inline destination backend reusing the source password, these scenarios
//! prove the snapshot-level story end-to-end: two independent filesystem
//! repositories with their own passwords, copies materialized as
//! `origin: replicated` `Snapshot` CRs at the destination, catalog-refresh
//! dedup (a copy never comes back as a `discovered` row), idempotent re-runs,
//! coexistence with the destination's own direct backups (GFS isolation in both
//! directions — the #368 consolidation claim), identity selection + `latestOnly`,
//! `mirrorSource` pruning, and `suspend`.
//!
//! ## Destination mount path (`/repo-dst`)
//!
//! An fs→fs replication mover mounts BOTH repositories in ONE pod: the source
//! read-only at its `backend.path`, the destination read-write at its own
//! `backend.path`. Two repos both at the default `/repo` would put two volumes
//! at one `mountPath` — so every destination repo here pins
//! `backend.filesystem.path: /repo-dst` (the same workaround the
//! `RepositoryReplication` sibling uses for its inline destination). kopia
//! writes the repo at the PVC *root* regardless of the mount path, so
//! verifiers over the same subpath still read it at the default `/repo`.
//! NOTE (operator-side observation, reported with this suite): nothing at
//! admission rejects the colliding same-path fs→fs shape — the webhook's
//! `backend_target_key` deliberately keys filesystem targets by *volume*, not
//! path — even though `spawn_snapshot_replication_job` assumes "the webhook
//! keeps [paths] from colliding".
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use kube::{Api, Client, ResourceExt};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ConfigMap;

use kopiur_api::consts::{
    CONFIG_LABEL, ORIGIN_LABEL, REPOSITORY_UID_LABEL, SNAPSHOT_ID_LABEL, SNAPSHOT_REPLICATION_LABEL,
};
use kopiur_api::{Repository, Snapshot, SnapshotPolicy, SnapshotReplication};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

/// In-pod mount path every DESTINATION repository pins (see module docs).
const DEST_REPO_PATH: &str = "/repo-dst";

/// Single-flight Job selector for a `SnapshotReplication`'s mover Jobs. The
/// component value is a wire contract with the CONTROLLER crate
/// (`SNAPSHOT_REPLICATION_COMPONENT`) — a deliberate literal here, like the
/// `replication` selector in tests/replication.rs: an accidental rename must
/// fail this suite.
fn srepl_job_selector(name: &str) -> String {
    format!("app.kubernetes.io/component=snapshot-replication,{SNAPSHOT_REPLICATION_LABEL}={name}")
}

/// The `Repository` API in the e2e namespace.
fn repos_api(client: &Client) -> Api<Repository> {
    Api::namespaced(client.clone(), E2E_NAMESPACE)
}

/// This replication's copy `Snapshot` CRs at the destination — the exact
/// three-label conjunction the mover stamps at birth AND uses as its pruning
/// candidate set (labels-at-birth contract).
async fn copy_rows(client: &Client, repl: &str, dest_uid: &str) -> Vec<Snapshot> {
    let api: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let selector = format!(
        "{SNAPSHOT_REPLICATION_LABEL}={repl},{ORIGIN_LABEL}=replicated,{REPOSITORY_UID_LABEL}={dest_uid}"
    );
    api.list(&ListParams::default().labels(&selector))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

/// A repository's `origin: discovered` catalog rows (the dedup labels the
/// catalog scan stamps) — same helper shape as tests/adoption.rs.
async fn discovered_rows(client: &Client, ns: &str, repo_uid: &str) -> Vec<Snapshot> {
    let api: Api<Snapshot> = Api::namespaced(client.clone(), ns);
    let selector = format!("{ORIGIN_LABEL}=discovered,{REPOSITORY_UID_LABEL}={repo_uid}");
    api.list(&ListParams::default().labels(&selector))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

/// A policy's config-labeled (directly-produced) `Snapshot` CRs.
async fn config_labeled(client: &Client, policy: &str) -> Vec<Snapshot> {
    let api: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    api.list(&ListParams::default().labels(&format!("{CONFIG_LABEL}={policy}")))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

/// The kopia snapshot id recorded on a `Snapshot` (`status.snapshot.kopiaSnapshotID`).
fn kopia_id(s: &Snapshot) -> String {
    serde_json::to_value(s)
        .unwrap_or_default()
        .pointer("/status/snapshot/kopiaSnapshotID")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// A copy CR's `status.copiedFrom.sourceManifestId` (the SOURCE-side lineage).
fn copied_from_source_id(s: &Snapshot) -> String {
    serde_json::to_value(s)
        .unwrap_or_default()
        .pointer("/status/copiedFrom/sourceManifestId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Wait until `status.lastReplicated` is a non-empty string, returning the FULL
/// status observed in that same poll — so the `lastRun` counters asserted by the
/// caller are the ones that accompanied that `lastReplicated`, not a later run's.
async fn wait_last_replicated(
    repls: &Api<SnapshotReplication>,
    name: &str,
    what: &str,
) -> serde_json::Value {
    wait_until(what, default_timeout(), poll_interval(), || async {
        let s = status_json(repls, name).await;
        let ok = s
            .get("lastReplicated")
            .and_then(|v| v.as_str())
            .is_some_and(|t| !t.is_empty());
        Ok(ok.then_some(s))
    })
    .await
    .unwrap_or_else(|e| panic!("{what}: {e}"))
}

/// Provision an fs destination repository over its own `subpath` at
/// [`DEST_REPO_PATH`] with `creds_secret`, merged with `extra_spec`, and wait
/// for it to bootstrap Ready.
async fn ensure_dest_repo(
    client: &Client,
    name: &str,
    subpath: &str,
    creds_secret: &str,
    extra_spec: serde_json::Value,
) {
    ensure_repo(client, subpath).await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repos,
        &cr(repository_json_with(
            name,
            subpath,
            DEST_REPO_PATH,
            creds_secret,
            extra_spec,
        )),
        "create destination Repository",
    )
    .await;
    wait_phase(&repos, name, "Ready")
        .await
        .expect("destination repository should bootstrap to Ready");
}

/// The flagship scenario: full-history fs→fs replication between two
/// repositories with DIFFERENT passwords.
///
/// 1. Source repo (`srepl-src`) + policy + TWO Snapshots (two runs — the
///    default path writes a distinct manifest per run, pinned by
///    tests/unchanged_snapshot.rs).
/// 2. Destination repo (`srepl-dst`) with its OWN password Secret — reaching a
///    successful run therefore proves the dest password reached the mover via
///    `KOPIUR_DEST_KOPIA_PASSWORD` and the persisted-source-password probe held
///    (the two-password `kopia snapshot migrate` contract).
/// 3. The mover Job's inline work-spec env carries
///    `/operation/snapshotReplicate` with the resolved destination and every
///    optional knob elided at its default — the controller↔mover seam.
/// 4. Copies materialize as `origin: replicated` Snapshot CRs with the
///    labels-at-birth set, the `spec.repository` destination pin,
///    `deletionPolicy: Delete`, `phase: Succeeded`, and `status.copiedFrom`
///    lineage back to the exact source manifests.
/// 5. Catalog-no-dup: a forced dest catalog refresh must NOT re-materialize the
///    copies as `discovered` rows (the label-arm suppression).
/// 6. Idempotent second run: `snapshotsCopied == 0`, `alreadyPresent == 2`.
/// 7. Destination really holds the data (independent verifier connect).
/// 8. Leak/maintenance guards: no per-run work-spec ConfigMap (#224 class), and
///    no k8s maintenance Job ever ran for the destination repo. (kopia-side
///    auto-maintenance suppression — migrate's CLI would run it, kopiur passes
///    `--no-auto-maintenance` — is in-process and invisible to Jobs; it is
///    pinned by the kopia-crate migrate integration tests.)
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn snapshot_replication_copies_history_between_filesystem_repos() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    const REPO_SRC: &str = "e2e-srepl-src";
    const POLICY: &str = "e2e-srepl-pol";
    const B1: &str = "e2e-srepl-b1";
    const B2: &str = "e2e-srepl-b2";
    const REPO_DST: &str = "e2e-srepl-dst";
    const REPL: &str = "e2e-srepl";
    const DST_SECRET: &str = "kopia-srepl-dst-creds";
    // DIFFERENT from consts::KOPIA_PASSWORD — the whole point of this fixture.
    const DST_PASSWORD: &str = "e2e-srepl-dest-password-456";

    // 1. Source history: seed (repo + policy + snapshot 1), then a second
    //    distinct backup under the same policy.
    ensure_seed(&client, REPO_SRC, POLICY, B1, "srepl-src").await;
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &backups,
        &cr(snapshot_json(
            E2E_NAMESPACE,
            B2,
            POLICY,
            serde_json::json!({}),
        )),
        "create second source Snapshot",
    )
    .await;
    wait_phase(&backups, B2, "Succeeded")
        .await
        .expect("second source Snapshot should Succeed");
    let src_ids: BTreeSet<String> = {
        let mut ids = BTreeSet::new();
        for b in [B1, B2] {
            let id = kopia_id(&backups.get(b).await.expect("get source Snapshot"));
            assert!(!id.is_empty(), "source Snapshot {b} must own a kopia id");
            ids.insert(id);
        }
        assert_eq!(ids.len(), 2, "two runs must yield two DISTINCT manifests");
        ids
    };

    // 2. Destination repo with its OWN password Secret. Managed maintenance is
    //    DISABLED outright: a cron pin cannot make the guard below flake-free,
    //    because the scheduling kernel anchors a never-run Maintenance CR at
    //    now-365d — the FIRST slot of any cron is always immediately due, so a
    //    "Jan-1-only" schedule still fires once right after the repo comes up
    //    (observed in-cluster as `<repo>-f-1767225600`). With maintenance off,
    //    any maintenance Job for the dest could only come from the
    //    replication path — the exact thing the guard exists to catch.
    {
        use kopiur_e2e::apply::{Fixture, apply_all};
        use kopiur_e2e::builders;
        let fixtures: Vec<Fixture> = vec![
            builders::opaque_secret(
                E2E_NAMESPACE,
                DST_SECRET,
                &[("KOPIA_PASSWORD", DST_PASSWORD)],
            )
            .into(),
        ];
        apply_all(&client, &fixtures)
            .await
            .expect("provision the destination-password Secret");
    }
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    ensure_dest_repo(
        &client,
        REPO_DST,
        "srepl-dst",
        DST_SECRET,
        serde_json::json!({
            "maintenance": { "enabled": false }
        }),
    )
    .await;

    // 3. The replication: full history (no selection), pruning default (none),
    //    an every-minute cron (no jitter) so runs happen promptly.
    let repls: Api<SnapshotReplication> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repls,
        &cr(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotReplication",
            "metadata": { "name": REPL, "namespace": E2E_NAMESPACE },
            "spec": {
                "sourceRef": { "kind": "Repository", "name": REPO_SRC },
                "destinationRef": { "kind": "Repository", "name": REPO_DST },
                "schedule": { "cron": "* * * * *" }
            }
        })),
        "create SnapshotReplication",
    )
    .await;

    // The per-slot mover Job's inline work-spec env is the controller↔mover
    // contract seam (mirrors the replication.rs #216 guard): the operation must
    // be snapshotReplicate, carry the RESOLVED destination, and elide every
    // optional knob left at its default.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let selector = srepl_job_selector(REPL);
    let job = wait_until(
        "a snapshot-replication mover Job is created",
        default_timeout(),
        poll_interval(),
        || async {
            let list = jobs.list(&ListParams::default().labels(&selector)).await?;
            Ok(list.items.into_iter().next())
        },
    )
    .await
    .expect("a snapshot-replication mover Job should be created");
    let spec = work_spec_json_from_job(&job);
    let op = spec
        .pointer("/operation/snapshotReplicate")
        .unwrap_or_else(|| panic!("work spec must carry operation.snapshotReplicate: {spec}"));
    assert_eq!(
        op.pointer("/destinationRepository/kind")
            .and_then(|v| v.as_str()),
        Some("Repository"),
        "op: {op}"
    );
    assert_eq!(
        op.pointer("/destinationRepository/name")
            .and_then(|v| v.as_str()),
        Some(REPO_DST),
        "op: {op}"
    );
    assert!(
        op.pointer("/destinationRepository/uid")
            .and_then(|v| v.as_str())
            .is_some_and(|u| !u.is_empty()),
        "the resolved destination uid rides the wire (copy-CR label input): {op}"
    );
    assert_eq!(
        op.pointer("/sourceRepository/name")
            .and_then(|v| v.as_str()),
        Some(REPO_SRC),
        "op: {op}"
    );
    assert!(
        op.get("destination").is_some(),
        "the destination backend connect block must ride the wire: {op}"
    );
    // Defaults are ELIDED on the wire (serde skip_serializing_if) — an
    // unexpected key here means a default silently changed shape.
    for absent in [
        "include",
        "exclude",
        "latestOnly",
        "parallel",
        "policies",
        "pruning",
    ] {
        assert!(
            op.get(absent).is_none(),
            "unset knob {absent} must be elided from the wire: {op}"
        );
    }

    // 4. A run succeeds and records lastReplicated + the run counters. The
    //    counter assert tolerates an in-place nextest retry (copies may already
    //    exist): copied+alreadyPresent must cover BOTH source manifests either way.
    let s1 = wait_last_replicated(
        &repls,
        REPL,
        "SnapshotReplication records a successful run (status.lastReplicated)",
    )
    .await;
    let run1 = &s1["lastRun"];
    assert_eq!(
        run1.get("failed").and_then(|v| v.as_u64()),
        Some(0),
        "a clean run reports failed=0 (post-verify caught nothing): {s1}"
    );
    assert_eq!(
        run1.get("identitiesSelected").and_then(|v| v.as_u64()),
        Some(1),
        "the source holds exactly one identity: {s1}"
    );
    let copied = run1
        .get("snapshotsCopied")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let already = run1
        .get("alreadyPresent")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert_eq!(
        copied + already,
        2,
        "the run must account for both source snapshots (copied {copied} + alreadyPresent {already}): {s1}"
    );

    // kstatus two-pass heal guard: the mover stamps the terminal status, the
    // controller heals Ready=True on a FOLLOW-UP reconcile — gate on the healed
    // condition like every sibling.
    wait_ready(&repls, REPL)
        .await
        .expect("a replication that recorded a successful run must heal kstatus Ready=True");

    // 5. Copy CRs: origin=replicated + the full label/spec/status contract.
    let dest_uid = repos
        .get(REPO_DST)
        .await
        .expect("get destination Repository")
        .uid()
        .expect("destination Repository has a uid");
    let rows = wait_until(
        "both copies materialize as replicated Snapshot CRs",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            let dest_uid = dest_uid.clone();
            async move {
                let rows = copy_rows(&client, REPL, &dest_uid).await;
                Ok((rows.len() == 2).then_some(rows))
            }
        },
    )
    .await
    .expect("exactly one copy CR per source snapshot should materialize at the destination");
    let mut copied_source_ids = BTreeSet::new();
    for row in &rows {
        let v = serde_json::to_value(row).unwrap_or_default();
        let name = row.name_any();
        // Labels at birth: the dest manifest id label matches the status id.
        let dest_id = kopia_id(row);
        assert!(
            !dest_id.is_empty(),
            "{name}: copy owns a dest kopia id: {v}"
        );
        assert_eq!(
            row.labels().get(SNAPSHOT_ID_LABEL).map(String::as_str),
            Some(dest_id.as_str()),
            "{name}: SNAPSHOT_ID_LABEL must match status.snapshot.kopiaSnapshotID: {v}"
        );
        // Spec: destination pin at create + Delete ownership, never a policy child.
        assert_eq!(
            v.pointer("/spec/repository/kind").and_then(|x| x.as_str()),
            Some("Repository"),
            "{name}: {v}"
        );
        assert_eq!(
            v.pointer("/spec/repository/name").and_then(|x| x.as_str()),
            Some(REPO_DST),
            "{name}: the spec.repository pin must name the DESTINATION: {v}"
        );
        assert_eq!(
            v.pointer("/spec/deletionPolicy").and_then(|x| x.as_str()),
            Some("Delete"),
            "{name}: a copy CR owns its migrated manifest: {v}"
        );
        assert!(
            v.pointer("/spec/policyRef").is_none(),
            "{name}: a copy CR is never a policy child: {v}"
        );
        // Status: terminal, replicated, with copiedFrom lineage.
        assert_eq!(
            v.pointer("/status/phase").and_then(|x| x.as_str()),
            Some("Succeeded"),
            "{name}: {v}"
        );
        assert_eq!(
            v.pointer("/status/origin").and_then(|x| x.as_str()),
            Some("replicated"),
            "{name}: {v}"
        );
        assert_eq!(
            v.pointer("/status/copiedFrom/repository/name")
                .and_then(|x| x.as_str()),
            Some(REPO_SRC),
            "{name}: copiedFrom names the SOURCE repository: {v}"
        );
        assert!(
            v.pointer("/status/copiedFrom/startTime")
                .and_then(|x| x.as_str())
                .is_some_and(|t| !t.is_empty()),
            "{name}: copiedFrom carries the manifest startTime: {v}"
        );
        let src_id = copied_from_source_id(row);
        assert!(
            !src_id.is_empty(),
            "{name}: copiedFrom.sourceManifestId: {v}"
        );
        copied_source_ids.insert(src_id);
        // No ownerReferences: deleting the SnapshotReplication never GCs copies.
        assert!(
            v.pointer("/metadata/ownerReferences").is_none(),
            "{name}: copy CRs must carry NO ownerReference: {v}"
        );
    }
    assert_eq!(
        copied_source_ids, src_ids,
        "copiedFrom lineage must cover EXACTLY the two source manifests"
    );

    // 6. Catalog-no-dup under a forced refresh: flip the dest catalog to a fast
    //    periodic refresh; once a scan that ran AFTER the copies landed is
    //    observable, the copies must NOT have come back as discovered rows —
    //    the origin=replicated label-arm suppression.
    let patched_at = chrono::Utc::now();
    repos
        .patch(
            REPO_DST,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "spec": { "catalog": { "periodicRefresh": true, "refreshInterval": "30s" } }
            })),
        )
        .await
        .expect("patch dest catalog to a fast periodic refresh");
    wait_until(
        "a dest catalog refresh runs after the copies landed",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&repos, REPO_DST).await;
            let refreshed = s
                .pointer("/catalog/lastRefreshAt")
                .and_then(|v| v.as_str())
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .is_some_and(|t| t.with_timezone(&chrono::Utc) > patched_at);
            // The scan seeing both manifests is an equally valid signal.
            let scanned = s
                .pointer("/storageStats/snapshotCount")
                .and_then(|v| v.as_i64())
                == Some(2);
            Ok((refreshed || scanned).then_some(()))
        },
    )
    .await
    .expect("catalog.refreshInterval=30s must re-scan the destination after replication");
    let disc = discovered_rows(&client, E2E_NAMESPACE, &dest_uid).await;
    assert!(
        disc.is_empty(),
        "a replicated copy must NEVER re-materialize as a discovered row; got {:?}",
        disc.iter().map(|d| d.name_any()).collect::<Vec<_>>()
    );

    // 7. Idempotent second run (the every-minute cron fires again naturally):
    //    lastReplicated advances and the counters flip to alreadyPresent.
    let first_stamp = s1["lastReplicated"].as_str().unwrap_or("").to_string();
    let s2 = wait_until(
        "a second replication run advances lastReplicated",
        default_timeout(),
        poll_interval(),
        || {
            let repls = repls.clone();
            let first_stamp = first_stamp.clone();
            async move {
                let s = status_json(&repls, REPL).await;
                let advanced = s
                    .get("lastReplicated")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| !t.is_empty() && t != first_stamp);
                Ok(advanced.then_some(s))
            }
        },
    )
    .await
    .expect("the cron should drive a second run");
    let run2 = &s2["lastRun"];
    assert_eq!(
        run2.get("snapshotsCopied").and_then(|v| v.as_u64()),
        Some(0),
        "an unchanged source must copy NOTHING on the second run: {s2}"
    );
    assert_eq!(
        run2.get("alreadyPresent").and_then(|v| v.as_u64()),
        Some(2),
        "the second run must recognize both copies as already present: {s2}"
    );
    assert_eq!(run2.get("failed").and_then(|v| v.as_u64()), Some(0), "{s2}");
    // Still exactly two copy CRs — the second run duplicated nothing.
    let rows_after = copy_rows(&client, REPL, &dest_uid).await;
    assert_eq!(
        rows_after.len(),
        2,
        "an idempotent re-run must not mint duplicate copy CRs"
    );

    // 8. The destination REALLY holds the data: an independent ReadOnly
    //    verifier connects with the DEST password and counts both manifests.
    let count =
        observed_snapshot_count_with_creds(&client, "e2e-srepl-verify", "srepl-dst", DST_SECRET)
            .await;
    assert_eq!(
        count, 2,
        "the destination repository must hold exactly the two migrated snapshots, got {count}"
    );

    // 9. Leak guard (#224 class): a replication slot creates NO per-run
    //    work-spec ConfigMap — the spec rides the Job env.
    for job in jobs
        .list(&ListParams::default().labels(&selector))
        .await
        .expect("list snapshot-replication Jobs")
        .items
    {
        let Some(job_name) = job.metadata.name else {
            continue;
        };
        assert!(
            cms.get_opt(&job_name)
                .await
                .expect("query ConfigMap")
                .is_none(),
            "replication slot {job_name} must not create a per-run work-spec ConfigMap"
        );
    }

    // 10. No maintenance ran for the DESTINATION as a k8s Job (its managed
    //     Maintenance CR shares the repo's name; managed maintenance is
    //     disabled above, so any Job here could only have come from the
    //     replication path). kopia-side auto-maintenance (which migrate's CLI
    //     would run) never surfaces as a Job — its suppression via
    //     `--no-auto-maintenance` is pinned by the kopia-crate integration tests.
    let maint_jobs = jobs
        .list(&ListParams::default().labels(&format!(
            "app.kubernetes.io/component=maintenance,kopiur.home-operations.com/maintenance={REPO_DST}"
        )))
        .await
        .expect("list maintenance Jobs for the destination")
        .items;
    assert!(
        maint_jobs.is_empty(),
        "no maintenance Job may run for the destination during replication; got {:?}",
        maint_jobs
            .iter()
            .filter_map(|j| j.metadata.name.clone())
            .collect::<Vec<_>>()
    );

    let _ = repls.delete(REPL, &DeleteParams::default()).await;
}

/// Coexistence (the #368 consolidation claim): a destination that is ALSO a
/// live direct-backup target. Both directions must hold:
/// - the direct policy's GFS retention (`keepLatest: 1`, Delete) prunes ONLY
///   its own config-labeled rows — the replicated copy CR and its manifest
///   survive untouched;
/// - the replication's copy set never absorbs the direct rows (label
///   conjunction: copies carry `SNAPSHOT_REPLICATION_LABEL`, direct rows carry
///   `CONFIG_LABEL`, and their kopia ids are disjoint).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn snapshot_replication_coexists_with_direct_backups_at_destination() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    const REPO_SRC: &str = "e2e-srepl-coex-src";
    const SPOL: &str = "e2e-srepl-coex-spol";
    const SB1: &str = "e2e-srepl-coex-sb1";
    const REPO_DST: &str = "e2e-srepl-coex-dst";
    const DPOL: &str = "e2e-srepl-coex-dpol";
    const D1: &str = "e2e-srepl-coex-d1";
    const D2: &str = "e2e-srepl-coex-d2";
    const REPL: &str = "e2e-srepl-coex";

    // Source with one snapshot; destination on the SHARED password (the
    // different-password contract is scenario 1's job).
    ensure_seed(&client, REPO_SRC, SPOL, SB1, "srepl-coex-src").await;
    ensure_dest_repo(
        &client,
        REPO_DST,
        "srepl-coex-dst",
        CREDS_SECRET,
        serde_json::json!({}),
    )
    .await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups_src_id = kopia_id(&backups.get(SB1).await.expect("get source Snapshot"));

    // The destination's OWN direct policy: distinct identity, tight GFS
    // (keepLatest: 1) with Delete so the prune really removes kopia data.
    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            E2E_NAMESPACE,
            DPOL,
            "Repository",
            REPO_DST,
            serde_json::json!({
                "identity": { "username": "coexdirect", "hostname": "e2e" },
                "defaultDeletionPolicy": "Delete",
                "retention": { "keepLatest": 1 }
            }),
        )),
        "create direct SnapshotPolicy at the destination",
    )
    .await;
    create_idempotent(
        &backups,
        &cr(snapshot_json(
            E2E_NAMESPACE,
            D1,
            DPOL,
            serde_json::json!({ "deletionPolicy": "Delete" }),
        )),
        "create first direct backup",
    )
    .await;
    wait_phase(&backups, D1, "Succeeded")
        .await
        .expect("first direct backup should Succeed");
    let d1_id = kopia_id(&backups.get(D1).await.expect("get D1"));

    // The replication (full history, pruning none). The webhook WARNS about the
    // dest-side policy overlap (match-all selection) but admits — mirrorSource
    // is the only denied combination.
    let repls: Api<SnapshotReplication> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repls,
        &cr(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotReplication",
            "metadata": { "name": REPL, "namespace": E2E_NAMESPACE },
            "spec": {
                "sourceRef": { "kind": "Repository", "name": REPO_SRC },
                "destinationRef": { "kind": "Repository", "name": REPO_DST },
                "schedule": { "cron": "* * * * *" }
            }
        })),
        "create SnapshotReplication",
    )
    .await;
    wait_last_replicated(&repls, REPL, "coexistence replication records a run").await;
    wait_ready(&repls, REPL)
        .await
        .expect("the coexistence replication must heal Ready=True");

    let dest_uid = repos
        .get(REPO_DST)
        .await
        .expect("get destination Repository")
        .uid()
        .expect("uid");
    let copies = wait_until(
        "the source snapshot materializes as one copy CR",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            let dest_uid = dest_uid.clone();
            async move {
                let rows = copy_rows(&client, REPL, &dest_uid).await;
                Ok((rows.len() == 1).then_some(rows))
            }
        },
    )
    .await
    .expect("exactly one copy CR for the single source snapshot");
    let copy_uid = copies[0].uid().expect("copy CR uid");
    let copy_kopia_id = kopia_id(&copies[0]);
    assert_eq!(
        copied_from_source_id(&copies[0]),
        backups_src_id,
        "the copy's lineage must point at the source manifest"
    );

    // Second direct backup → GFS (keepLatest: 1) prunes D1: CR deleted AND its
    // kopia manifest removed (Delete + operator prune, breaker-exempt).
    create_idempotent(
        &backups,
        &cr(snapshot_json(
            E2E_NAMESPACE,
            D2,
            DPOL,
            serde_json::json!({ "deletionPolicy": "Delete" }),
        )),
        "create second direct backup",
    )
    .await;
    wait_phase(&backups, D2, "Succeeded")
        .await
        .expect("second direct backup should Succeed");
    let d2_id = kopia_id(&backups.get(D2).await.expect("get D2"));
    wait_until(
        "GFS converges the direct rows to exactly the newest (D1 fully drained)",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            async move {
                let rows = config_labeled(&client, DPOL).await;
                let converged = rows.len() == 1
                    && rows[0].name_any() == D2
                    && rows[0].metadata.deletion_timestamp.is_none();
                Ok(converged.then_some(()))
            }
        },
    )
    .await
    .expect("keepLatest: 1 must prune the older direct row (and ONLY direct rows)");

    // Direction 1: the replicated copy survived the direct prune, untouched.
    let copies_after = copy_rows(&client, REPL, &dest_uid).await;
    assert_eq!(
        copies_after.len(),
        1,
        "the copy CR set must be untouched by direct GFS"
    );
    assert_eq!(
        copies_after[0].uid(),
        Some(copy_uid.clone()),
        "the SAME copy CR must survive (not a delete/recreate)"
    );
    let v = serde_json::to_value(&copies_after[0]).unwrap_or_default();
    assert_eq!(
        v.pointer("/status/phase").and_then(|x| x.as_str()),
        Some("Succeeded"),
        "{v}"
    );

    // Direction 2: the copy set never absorbs direct rows — disjoint labels AND
    // disjoint kopia ids.
    assert!(
        copies_after[0].labels().get(CONFIG_LABEL).is_none(),
        "a copy CR must never carry a policy config label"
    );
    assert!(
        copy_kopia_id != d1_id && copy_kopia_id != d2_id,
        "the copy's manifest must be distinct from every direct manifest"
    );
    let d2_row = backups.get(D2).await.expect("get D2");
    assert!(
        d2_row.labels().get(SNAPSHOT_REPLICATION_LABEL).is_none(),
        "a direct row must never carry the replication label"
    );

    // kopia-side proof for both directions at once: the destination holds
    // exactly the surviving direct manifest + the replicated copy. (D1's
    // manifest really pruned; the copy's manifest really intact.)
    let count = observed_snapshot_count(&client, "e2e-srepl-coex-verify", "srepl-coex-dst").await;
    assert_eq!(
        count, 2,
        "destination must hold exactly (1 direct kept + 1 replicated copy), got {count}"
    );

    let _ = repls.delete(REPL, &DeleteParams::default()).await;
}

/// Selection + `latestOnly`, then a live switch to `mirrorSource` pruning.
///
/// Source holds TWO identities × two snapshots each; the replication includes
/// only ONE identity with `latestOnly: true` ⇒ exactly one copy CR (the
/// included identity's newest). Switching `spec.pruning` to `mirrorSource` and
/// deleting that source snapshot (deletionPolicy Delete, so the manifest goes)
/// converges the next runs to: the stale copy PRUNED, the new latest copied,
/// and the destination holding exactly one manifest.
///
/// Deliberately OMITTED here: the bulk-source-vanish breaker hold
/// (`PruneHeldByBreaker`) — it needs ≥ deletionProtection.threshold (10)
/// source deletions plus the ack machinery, which is wall-clock-expensive in
/// kind; the mirrorSource-deletes-classify-EXTERNAL behavior it rides on is
/// covered by the mover's pruning unit tests.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn snapshot_replication_selection_latest_only_and_mirror_prune() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    const REPO_SRC: &str = "e2e-srepl-sel-src";
    const REPO_DST: &str = "e2e-srepl-sel-dst";
    const POL_KEEP: &str = "e2e-srepl-keep-pol";
    const POL_SKIP: &str = "e2e-srepl-skip-pol";
    const REPL: &str = "e2e-srepl-sel";

    ensure_repo(&client, "srepl-sel-src").await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repos,
        &cr(repository_json(
            REPO_SRC,
            "srepl-sel-src",
            serde_json::json!({}),
        )),
        "create source Repository",
    )
    .await;
    wait_phase(&repos, REPO_SRC, "Ready")
        .await
        .expect("source repository Ready");

    // Two identities over the same source PVC — two policies with pinned,
    // distinct identities (the cheapest way to get 2 identities in one repo).
    for (pol, user) in [(POL_KEEP, "sreplkeep"), (POL_SKIP, "sreplskip")] {
        create_idempotent(
            &policies,
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                pol,
                "Repository",
                REPO_SRC,
                serde_json::json!({ "identity": { "username": user, "hostname": "e2e" } }),
            )),
            "create source SnapshotPolicy",
        )
        .await;
    }
    // 2 snapshots per identity. K2 (the included identity's LATEST — created
    // second) carries deletionPolicy Delete so the mirror-prune step can really
    // remove its manifest from the source.
    let mut ids = std::collections::BTreeMap::new();
    for (name, pol, extra) in [
        ("e2e-srepl-sel-k1", POL_KEEP, serde_json::json!({})),
        (
            "e2e-srepl-sel-k2",
            POL_KEEP,
            serde_json::json!({ "deletionPolicy": "Delete" }),
        ),
        ("e2e-srepl-sel-s1", POL_SKIP, serde_json::json!({})),
        ("e2e-srepl-sel-s2", POL_SKIP, serde_json::json!({})),
    ] {
        create_idempotent(
            &backups,
            &cr(snapshot_json(E2E_NAMESPACE, name, pol, extra)),
            "create source Snapshot",
        )
        .await;
        wait_phase(&backups, name, "Succeeded")
            .await
            .expect("source Snapshot should Succeed");
        ids.insert(
            name,
            kopia_id(&backups.get(name).await.expect("get Snapshot")),
        );
    }
    let k1_id = ids["e2e-srepl-sel-k1"].clone();
    let k2_id = ids["e2e-srepl-sel-k2"].clone();

    ensure_dest_repo(
        &client,
        REPO_DST,
        "srepl-sel-dst",
        CREDS_SECRET,
        serde_json::json!({}),
    )
    .await;

    // Include matcher for ONE identity + latestOnly ⇒ exactly one copy.
    let repls: Api<SnapshotReplication> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repls,
        &cr(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotReplication",
            "metadata": { "name": REPL, "namespace": E2E_NAMESPACE },
            "spec": {
                "sourceRef": { "kind": "Repository", "name": REPO_SRC },
                "destinationRef": { "kind": "Repository", "name": REPO_DST },
                "schedule": { "cron": "* * * * *" },
                "selection": {
                    "identities": { "include": [ { "username": "sreplkeep" } ] },
                    "latestOnly": true
                }
            }
        })),
        "create selective SnapshotReplication",
    )
    .await;
    let s1 = wait_last_replicated(&repls, REPL, "selective replication records a run").await;
    assert_eq!(
        s1.pointer("/lastRun/identitiesSelected")
            .and_then(|v| v.as_u64()),
        Some(1),
        "the include matcher must select exactly ONE of the two identities: {s1}"
    );

    let dest_uid = repos
        .get(REPO_DST)
        .await
        .expect("get destination Repository")
        .uid()
        .expect("uid");
    let rows = wait_until(
        "exactly one copy CR (included identity, latest only)",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            let dest_uid = dest_uid.clone();
            async move {
                let rows = copy_rows(&client, REPL, &dest_uid).await;
                Ok((rows.len() == 1).then_some(rows))
            }
        },
    )
    .await
    .expect("include+latestOnly must yield exactly one copy CR");
    let v = serde_json::to_value(&rows[0]).unwrap_or_default();
    assert_eq!(
        v.pointer("/status/snapshot/identity/username")
            .and_then(|x| x.as_str()),
        Some("sreplkeep"),
        "only the INCLUDED identity may be copied: {v}"
    );
    assert_eq!(
        copied_from_source_id(&rows[0]),
        k2_id,
        "latestOnly must copy the identity's NEWEST snapshot (k2)"
    );

    // Switch pruning to mirrorSource, then delete the copied source snapshot
    // (Delete policy ⇒ the source manifest really goes).
    repls
        .patch(
            REPL,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "pruning": { "mirrorSource": {} } } })),
        )
        .await
        .expect("patch spec.pruning to mirrorSource");
    backups
        .delete("e2e-srepl-sel-k2", &DeleteParams::default())
        .await
        .expect("delete the copied source Snapshot CR");
    wait_until(
        "the source Snapshot CR drains (finalizer deleted the source manifest)",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(backups
                .get_opt("e2e-srepl-sel-k2")
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("the deletionPolicy: Delete source CR should drain, manifest included");

    // Next run(s): the stale copy (k2) is mirror-pruned — its CR deleted and,
    // via the copy's own Delete finalizer, its dest manifest too — while
    // latestOnly copies the identity's NEW latest (k1). Converge on the final
    // state; the pruned CR must be FULLY gone (finalizer released ⇒ the dest
    // manifest delete completed).
    wait_until(
        "mirrorSource converges: k2's copy pruned, k1's copy present",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            let dest_uid = dest_uid.clone();
            let k1_id = k1_id.clone();
            async move {
                let rows = copy_rows(&client, REPL, &dest_uid).await;
                let converged = rows.len() == 1
                    && copied_from_source_id(&rows[0]) == k1_id
                    && rows[0].metadata.deletion_timestamp.is_none();
                Ok(converged.then_some(()))
            }
        },
    )
    .await
    .expect("mirrorSource must prune the vanished snapshot's copy and keep mirroring the latest");

    // The destination's manifest count reflects the prune: exactly one (k1's copy).
    let count = observed_snapshot_count(&client, "e2e-srepl-sel-verify", "srepl-sel-dst").await;
    assert_eq!(
        count, 1,
        "after the mirror prune the destination must hold exactly one manifest, got {count}"
    );

    let _ = repls.delete(REPL, &DeleteParams::default()).await;
}

/// `suspend: true` holds every slot: the CR reports `phase: Suspended` and no
/// mover Job is ever minted across a full cron period (both repos Ready, so the
/// ONLY thing holding the slots is the suspend gate).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn snapshot_replication_suspend_holds_slots() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    const REPO_SRC: &str = "e2e-srepl-susp-src";
    const REPO_DST: &str = "e2e-srepl-susp-dst";
    const REPL: &str = "e2e-srepl-susp";

    ensure_repo(&client, "srepl-susp-src").await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repos,
        &cr(repository_json(
            REPO_SRC,
            "srepl-susp-src",
            serde_json::json!({}),
        )),
        "create source Repository",
    )
    .await;
    wait_phase(&repos, REPO_SRC, "Ready")
        .await
        .expect("source repository Ready");
    ensure_dest_repo(
        &client,
        REPO_DST,
        "srepl-susp-dst",
        CREDS_SECRET,
        serde_json::json!({}),
    )
    .await;

    let repls: Api<SnapshotReplication> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repls,
        &cr(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotReplication",
            "metadata": { "name": REPL, "namespace": E2E_NAMESPACE },
            "spec": {
                "sourceRef": { "kind": "Repository", "name": REPO_SRC },
                "destinationRef": { "kind": "Repository", "name": REPO_DST },
                "schedule": { "cron": "* * * * *" },
                "suspend": true
            }
        })),
        "create suspended SnapshotReplication",
    )
    .await;
    wait_phase(&repls, REPL, "Suspended")
        .await
        .expect("a suspended replication must report phase: Suspended");

    // Settle window: > one full every-minute cron period. No mover Job may
    // ever appear — poll the invariant throughout, adoption-style.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let selector = srepl_job_selector(REPL);
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let minted = jobs
            .list(&ListParams::default().labels(&selector))
            .await
            .expect("list snapshot-replication Jobs")
            .items;
        assert!(
            minted.is_empty(),
            "a suspended replication must mint NO mover Jobs; got {:?}",
            minted
                .iter()
                .filter_map(|j| j.metadata.name.clone())
                .collect::<Vec<_>>()
        );
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(poll_interval()).await;
    }

    let _ = repls.delete(REPL, &DeleteParams::default()).await;
}

/// Issue #380: an ON-DEMAND copy pass, requested with the `run-requested`
/// annotation, on a replication whose cron will not fire for months.
///
/// The far-future schedule (`0 5 1 1 *` — 05:00 on 1 January) is what makes
/// this a proof rather than a coincidence: no slot can come due, so a stamped
/// `lastReplicated` can only have come from the annotation. It pins the halves
/// a unit test cannot reach — the requested run rides the ordinary two-password
/// `kopia snapshot migrate` path (copies really land at the destination), and
/// `status.manualRun` answers the exact timestamp requested.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn snapshot_replication_runs_on_demand_from_the_run_requested_annotation() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    const REPO_SRC: &str = "e2e-srepl-run-src";
    const REPO_DST: &str = "e2e-srepl-run-dst";
    const REPL: &str = "e2e-srepl-run";

    // A source with real snapshot history to copy.
    ensure_seed(
        &client,
        REPO_SRC,
        "e2e-srepl-run-policy",
        "e2e-srepl-run-seed",
        "srepl-run-src",
    )
    .await;
    ensure_dest_repo(
        &client,
        REPO_DST,
        "srepl-run-dst",
        CREDS_SECRET,
        serde_json::json!({}),
    )
    .await;

    let repls: Api<SnapshotReplication> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repls,
        &cr(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotReplication",
            "metadata": { "name": REPL, "namespace": E2E_NAMESPACE },
            "spec": {
                "sourceRef": { "kind": "Repository", "name": REPO_SRC },
                "destinationRef": { "kind": "Repository", "name": REPO_DST },
                // Months away: no cron slot can fire during this test.
                "schedule": { "cron": "0 5 1 1 *" }
            }
        })),
        "create SnapshotReplication with a far-future schedule",
    )
    .await;

    // Nothing should have run yet — otherwise the assertion below would pass on
    // a cron slot and prove nothing about the annotation.
    let before = status_json(&repls, REPL).await;
    assert!(
        before.get("lastReplicated").is_none(),
        "a far-future schedule must not have replicated yet; status: {before}"
    );

    let requested_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    repls
        .patch(
            REPL,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "metadata": { "annotations": {
                    kopiur_api::consts::RUN_REQUESTED_ANNOTATION: requested_at
                } }
            })),
        )
        .await
        .expect("annotate the replication with a run request");

    wait_until(
        "an annotation-requested snapshot-replication run stamps status.lastReplicated",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&repls, REPL).await;
            Ok(s.get("lastReplicated")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|_| ()))
        },
    )
    .await
    .expect("the requested run should execute and stamp status.lastReplicated");

    let manual = wait_until(
        "status.manualRun reaches a terminal phase",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&repls, REPL).await;
            Ok(s.get("manualRun")
                .and_then(|m| m.get("phase").and_then(|p| p.as_str()).map(|_| m.clone())))
        },
    )
    .await
    .expect("the controller should record status.manualRun");
    assert_eq!(
        manual.get("phase").and_then(|v| v.as_str()),
        Some("Succeeded"),
        "manualRun: {manual}"
    );
    assert_eq!(
        manual.get("requestedAt").and_then(|v| v.as_str()),
        Some(requested_at.as_str()),
        "manualRun must pin the timestamp that was requested; got {manual}"
    );

    // The copies really landed: the requested run did exactly the work a
    // scheduled one would, through the same mover.
    let dest_uid = repos_api(&client)
        .get(REPO_DST)
        .await
        .expect("get destination Repository")
        .uid()
        .expect("destination Repository has a uid");
    let copies = wait_until(
        "the requested run's copies materialize as replicated Snapshot CRs",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            let dest_uid = dest_uid.clone();
            async move {
                let rows = copy_rows(&client, REPL, &dest_uid).await;
                Ok((!rows.is_empty()).then_some(rows))
            }
        },
    )
    .await
    .expect("a successful requested run must materialize copy Snapshot CRs at the destination");
    assert!(!copies.is_empty());

    let _ = repls.delete(REPL, &DeleteParams::default()).await;
}
