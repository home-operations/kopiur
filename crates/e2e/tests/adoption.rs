//! e2e: the **snapshot-adoption** story end-to-end (fixes #210) — a discovered
//! catalog entry whose kopia identity matches a live `SnapshotPolicy` is
//! *adopted* (re-attached as an `origin: adopted` `Snapshot` carrying the policy's
//! config label), then governed by GFS retention like any produced backup, instead
//! of sitting in the catalog forever.
//!
//! Scenario 1 (`policy_recreate_adopts_and_prunes_history`) is THE #210 acceptance
//! test: it delete-then-recreates a policy over real backdated history and proves
//! the recreated policy hands-off (a) requests an on-demand catalog scan, (b)
//! rediscovers the retained kopia snapshots, (c) adopts the identity-matching ones,
//! and (e) — the assertion that would have FAILED before this branch — actually
//! prunes kopia-side history down to the recreated policy's `keepLatest: 1`, while a
//! NON-matching seeded snapshot stays a forced-Retain `discovered` row (its kopia
//! data untouched). Scenario 2 proves the `catalog.adoption: Ignore` opt-out leaves
//! matching rows discovered. Scenario 3 proves cross-namespace `ClusterRepository`
//! adoption (the cluster-wide-LIST guard).
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; skip gracefully off-cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client, ResourceExt};

use k8s_openapi::api::events::v1::Event as EventsV1;

use kopiur_api::consts::{CONFIG_LABEL, ORIGIN_LABEL, REPOSITORY_UID_LABEL, SCHEDULE_LABEL};
use kopiur_api::{ClusterRepository, Repository, Snapshot, SnapshotPolicy, SnapshotSchedule};
use kopiur_e2e::builders::SeedStep;
use kopiur_e2e::{E2E_NAMESPACE, Need, World, consts, default_timeout, poll_interval, wait_until};

use common::{
    cr, ensure_repo, observed_snapshot_count, repository_json, snapshot_json, snapshot_policy_json,
    status_json, wait_phase,
};

// --- Wire-contract literals mirrored from the CONTROLLER crate --------------------
// The e2e crate does not depend on `kopiur-controller`, so the on-demand catalog-scan
// annotation and the adoption Event reason are DELIBERATE literals here — the same
// pattern `common::WORK_SPEC_ENV` / the batch-delete labels use: an accidental rename
// in the controller must fail THIS suite at runtime (these are a wire contract these
// scenarios read). `crate::consts::{CATALOG_SCAN_REQUESTED_ANNOTATION, SNAPSHOTS_ADOPTED_REASON}`.
const CATALOG_SCAN_REQUESTED_ANNOTATION: &str =
    "kopiur.home-operations.com/catalog-scan-requested-at";
const SNAPSHOTS_ADOPTED_REASON: &str = "SnapshotsAdopted";
const ADOPTION_SKIPPED_BY_RETENTION_REASON: &str = "AdoptionSkippedByRetention";

// --- shared helpers ---------------------------------------------------------------

/// A `SnapshotSchedule` firing `policy` on `cron` with `runOnCreate`.
fn schedule_json(name: &str, policy: &str, cron: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotSchedule",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "policyRef": { "name": policy },
            "schedule": { "cron": cron, "runOnCreate": true }
        }
    })
}

/// The kopia snapshot id recorded on a `Snapshot` (`status.snapshot.kopiaSnapshotID`),
/// or `""` if absent.
fn kopia_id(s: &Snapshot) -> String {
    serde_json::to_value(s)
        .unwrap_or_default()
        .pointer("/status/snapshot/kopiaSnapshotID")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// The `status.origin` recorded on a `Snapshot`, or `""` if absent.
fn status_origin(s: &Snapshot) -> String {
    serde_json::to_value(s)
        .unwrap_or_default()
        .pointer("/status/origin")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// The `username@hostname:sourcePath` identity recorded on a discovered/adopted row.
fn row_identity(s: &Snapshot) -> String {
    let v = serde_json::to_value(s).unwrap_or_default();
    let id = v
        .pointer("/status/snapshot/identity")
        .cloned()
        .unwrap_or_default();
    format!(
        "{}@{}:{}",
        id.get("username").and_then(|x| x.as_str()).unwrap_or(""),
        id.get("hostname").and_then(|x| x.as_str()).unwrap_or(""),
        id.get("sourcePath").and_then(|x| x.as_str()).unwrap_or(""),
    )
}

/// This repository's discovered rows in `ns`, via the dedup labels the catalog scan
/// stamps (`origin=discovered` + the repository UID).
async fn discovered_rows(client: &Client, ns: &str, repo_uid: &str) -> Vec<Snapshot> {
    let api: Api<Snapshot> = Api::namespaced(client.clone(), ns);
    let selector = format!("{ORIGIN_LABEL}=discovered,{REPOSITORY_UID_LABEL}={repo_uid}");
    api.list(&ListParams::default().labels(&selector))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

/// A policy's config-labeled `Snapshot` CRs in `ns`.
async fn config_labeled(client: &Client, ns: &str, policy: &str) -> Vec<Snapshot> {
    let api: Api<Snapshot> = Api::namespaced(client.clone(), ns);
    api.list(&ListParams::default().labels(&format!("{CONFIG_LABEL}={policy}")))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

/// Whether an `events.k8s.io/v1` Event with `reason` regarding `kind`/`name` exists
/// in `ns`. The event outlives the object, so it stays queryable after a CR delete.
async fn event_exists(client: &Client, ns: &str, reason: &str, kind: &str, name: &str) -> bool {
    let events: Api<EventsV1> = Api::namespaced(client.clone(), ns);
    let list = match events.list(&ListParams::default()).await {
        Ok(l) => l,
        Err(_) => return false,
    };
    list.items.iter().any(|e| {
        e.reason.as_deref() == Some(reason)
            && e.regarding
                .as_ref()
                .is_some_and(|r| r.kind.as_deref() == Some(kind) && r.name.as_deref() == Some(name))
    })
}

/// The repository's live `catalog-scan-requested-at` annotation value, if any.
async fn scan_requested_annotation(repos: &Api<Repository>, name: &str) -> Option<String> {
    repos
        .get_opt(name)
        .await
        .ok()
        .flatten()
        .and_then(|r| {
            r.annotations()
                .get(CATALOG_SCAN_REQUESTED_ANNOTATION)
                .cloned()
        })
        .filter(|v| !v.is_empty())
}

// --- Scenario 1: the #210 adopt-then-prune acceptance test ------------------------

const S1_SUBPATH: &str = "adopt-prune";

/// THE #210 ACCEPTANCE TEST. A filesystem repo + policy + schedule produce ≥3 REAL
/// distinct kopia snapshots; a fourth is seeded under a NON-matching identity (the
/// negative control). The schedule AND policy are deleted — every config-labeled CR
/// drains, the policy finalizer releases (cascade guard), and kopia is UNCHANGED
/// (safe-default Retain). The policy is then recreated with the SAME identity and a
/// tight `keepLatest: 1`; hands-off it (a) stamps `catalog-scan-requested-at`, (b)
/// rediscovers, (c) adopts the ≥3 matching rows (`origin=adopted`, config-labeled,
/// Succeeded), (d) records `lastAdoptedCount>=3` + a `SnapshotsAdopted` event, and
/// (e) — the assertion that would have FAILED before this branch, because adoption
/// did not exist and the rows would sit as immortal `discovered` entries — prunes
/// kopia history for the identity down to 1, while the non-matching control stays a
/// forced-Retain `discovered` row with its kopia snapshot intact.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn policy_recreate_adopts_and_prunes_history() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client: Client = world.client().clone();
    ensure_repo(&client, S1_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-adopt-repo";
    const POLICY: &str = "e2e-adopt-pol";
    const NEG_POLICY: &str = "e2e-adopt-neg-pol";
    const SCHED: &str = "e2e-adopt-sched";
    const NEG_SNAP: &str = "e2e-adopt-neg-1";

    // A fast catalog refresh so rediscovery doesn't wait an hour; maintenance off to
    // cut Job churn. No deletionProtection — adoption/prune are operator-internal.
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                S1_SUBPATH,
                serde_json::json!({
                    "maintenance": { "enabled": false },
                    "catalog": { "periodicRefresh": true, "refreshInterval": "30s" }
                }),
            )),
        )
        .await
        .expect("create Repository");
    wait_phase(&repos, REPO, "Ready")
        .await
        .expect("Repository should reach Ready");

    // Main recipe: generous retention so no scheduled snapshot is GFS-pruned during
    // seeding; `defaultDeletionPolicy: Delete` so the adopted rows (later) actually
    // delete their kopia data on prune — the #210 proof.
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                POLICY,
                "Repository",
                REPO,
                serde_json::json!({ "defaultDeletionPolicy": "Delete", "retention": { "keepLatest": 20 } }),
            )),
        )
        .await
        .expect("create main SnapshotPolicy");

    // The schedule that produces the history (honors "policy + schedule"; deleting it
    // with the policy also regression-guards the cascade).
    schedules
        .create(
            &PostParams::default(),
            &cr::<SnapshotSchedule>(schedule_json(SCHED, POLICY, "* * * * *")),
        )
        .await
        .expect("create SnapshotSchedule");

    // Collect ≥3 Succeeded scheduled snapshots carrying DISTINCT kopia ids.
    let sched_selector = format!("{SCHEDULE_LABEL}={SCHED}");
    let distinct_ids = wait_until(
        "schedule produces ≥3 Succeeded snapshots with distinct kopia ids",
        default_timeout(),
        poll_interval(),
        || {
            let backups = backups.clone();
            let sel = sched_selector.clone();
            async move {
                let list = backups
                    .list(&ListParams::default().labels(&sel))
                    .await?
                    .items;
                let ids: BTreeSet<String> = list
                    .iter()
                    .filter(|b| {
                        serde_json::to_value(b)
                            .unwrap_or_default()
                            .pointer("/status/phase")
                            .and_then(|p| p.as_str())
                            == Some("Succeeded")
                    })
                    .map(kopia_id)
                    .filter(|id| !id.is_empty())
                    .collect();
                Ok((ids.len() >= 3).then_some(ids))
            }
        },
    )
    .await
    .expect("the schedule should produce ≥3 Succeeded snapshots with distinct kopia ids");
    assert!(
        distinct_ids.len() >= 3,
        "need ≥3 distinct kopia snapshots; got {distinct_ids:?}"
    );

    // Freeze production, then let any in-flight run settle so the counts are stable.
    schedules
        .patch(
            SCHED,
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({ "spec": { "schedule": { "suspend": true } } })),
        )
        .await
        .expect("suspend schedule to freeze the produced set");
    // Wait until EVERY scheduled snapshot is in a TERMINAL phase — so no in-flight
    // mover run can still add a kopia snapshot after we count, keeping the produced
    // set and `kopia_before` stable (a fixed sleep can race a slow kind mover Job).
    wait_until(
        "all scheduled snapshots reach a terminal phase after suspend",
        default_timeout(),
        poll_interval(),
        || {
            let backups = backups.clone();
            let sel = sched_selector.clone();
            async move {
                let list = backups
                    .list(&ListParams::default().labels(&sel))
                    .await?
                    .items;
                let all_terminal = list.iter().all(|b| {
                    matches!(
                        serde_json::to_value(b)
                            .unwrap_or_default()
                            .pointer("/status/phase")
                            .and_then(|p| p.as_str()),
                        Some("Succeeded") | Some("Failed")
                    )
                });
                Ok(all_terminal.then_some(()))
            }
        },
    )
    .await
    .expect("scheduled snapshots should settle to terminal phases after suspend");

    // Backdate the produced snapshots across DISTINCT days (reuse the retention
    // primitive) so the original history spans GFS buckets — proving the seeded set
    // is real, calendar-spread history and not a degenerate same-instant burst.
    let produced: Vec<Snapshot> = backups
        .list(&ListParams::default().labels(&sched_selector))
        .await
        .expect("list produced snapshots")
        .items
        .into_iter()
        .filter(|b| {
            serde_json::to_value(b)
                .unwrap_or_default()
                .pointer("/status/phase")
                .and_then(|p| p.as_str())
                == Some("Succeeded")
        })
        .collect();
    for (i, b) in produced.iter().enumerate() {
        let day = format!("2026-03-{:02}T10:00:00Z", i + 1);
        let patch = serde_json::json!({ "status": { "timing": { "endTime": day } } });
        backups
            .patch_status(
                &b.name_any(),
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await
            .unwrap_or_else(|e| panic!("backdate {} endTime: {e}", b.name_any()));
    }
    let produced_count = produced.len();

    // The NEGATIVE CONTROL: one snapshot under a NON-matching identity. A second
    // policy whose name (⇒ username default) differs gives a distinct kopia identity
    // over the same repo; its snapshot must NEVER be adopted by the main policy.
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                NEG_POLICY,
                "Repository",
                REPO,
                serde_json::json!({ "retention": { "keepLatest": 20 } }),
            )),
        )
        .await
        .expect("create negative-control SnapshotPolicy");
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                NEG_SNAP,
                NEG_POLICY,
                serde_json::json!({ "deletionPolicy": "Retain" }),
            )),
        )
        .await
        .expect("create negative-control Snapshot");
    wait_phase(&backups, NEG_SNAP, "Succeeded")
        .await
        .expect("negative-control Snapshot should Succeed");
    let neg_identity = row_identity(
        &backups
            .get(NEG_SNAP)
            .await
            .expect("get negative-control Snapshot"),
    );

    // kopia holds the (≥3) produced snapshots + the 1 negative-control snapshot. Use
    // `>=` (not exact): all produced snapshots share the policy identity and are ALL
    // adopted-then-pruned to 1 later, so the load-bearing proof is the FINAL count
    // (`== 2`) and the UNCHANGED-across-delete count — not the exact starting value.
    let kopia_before = observed_snapshot_count(&client, "e2e-adopt-verify-1", S1_SUBPATH).await;
    assert!(
        kopia_before >= (produced_count + 1) as i64 && kopia_before >= 4,
        "kopia should hold the {produced_count} produced + 1 negative-control snapshot (≥4) before deletion; got {kopia_before}"
    );

    // --- Delete the schedule AND both policies (the cascade + finalizer guard) ---
    schedules
        .delete(SCHED, &DeleteParams::default())
        .await
        .expect("delete the SnapshotSchedule");
    policies
        .delete(POLICY, &DeleteParams::default())
        .await
        .expect("delete the main SnapshotPolicy");
    policies
        .delete(NEG_POLICY, &DeleteParams::default())
        .await
        .expect("delete the negative-control SnapshotPolicy");

    // Every config-labeled CR (both policies) drains, and BOTH policy CRs are gone —
    // the finalizer released (the cascade did not wedge). Generous deadline (kind).
    wait_until(
        "schedule + both policies + all their config-labeled Snapshot CRs drain",
        Duration::from_secs(360),
        poll_interval(),
        || async {
            let main_children = config_labeled(&client, E2E_NAMESPACE, POLICY).await;
            let neg_children = config_labeled(&client, E2E_NAMESPACE, NEG_POLICY).await;
            let pol_gone = policies.get_opt(POLICY).await?.is_none();
            let neg_gone = policies.get_opt(NEG_POLICY).await?.is_none();
            let sched_gone = schedules.get_opt(SCHED).await?.is_none();
            Ok((main_children.is_empty()
                && neg_children.is_empty()
                && pol_gone
                && neg_gone
                && sched_gone)
                .then_some(()))
        },
    )
    .await
    .expect("the policy-deletion cascade must drain every config-labeled CR and RELEASE both policy finalizers");

    // kopia UNCHANGED — the safe-default Retain cascade kept every snapshot.
    let kopia_after_delete =
        observed_snapshot_count(&client, "e2e-adopt-verify-2", S1_SUBPATH).await;
    assert_eq!(
        kopia_after_delete, kopia_before,
        "a Retain-cascade policy+schedule delete MUST NOT touch kopia data; before={kopia_before} after={kopia_after_delete}"
    );

    // --- Recreate the SAME policy with a tight keepLatest: 1 + default adoption ---
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                POLICY,
                "Repository",
                REPO,
                serde_json::json!({ "defaultDeletionPolicy": "Delete", "retention": { "keepLatest": 1 } }),
            )),
        )
        .await
        .expect("recreate the main SnapshotPolicy");

    let repo_uid = repos
        .get(REPO)
        .await
        .expect("get Repository")
        .uid()
        .expect("Repository uid");

    // (a) the recreated policy requests an on-demand catalog scan on its repository.
    wait_until(
        "recreated policy stamps catalog-scan-requested-at on its repository",
        default_timeout(),
        poll_interval(),
        || async { Ok(scan_requested_annotation(&repos, REPO).await.map(|_| ())) },
    )
    .await
    .expect(
        "the recreated policy must request an on-demand catalog scan (`catalog-scan-requested-at`)",
    );

    // (b) discovered rows materialize — proven by the persistent NEGATIVE CONTROL row
    // (never adopted, so it does not vanish under us like the matching rows do).
    wait_until(
        "the catalog rediscovers the non-matching (negative-control) snapshot as a discovered row",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            let repo_uid = repo_uid.clone();
            let neg_identity = neg_identity.clone();
            async move {
                let rows = discovered_rows(&client, E2E_NAMESPACE, &repo_uid).await;
                Ok(rows
                    .iter()
                    .any(|r| row_identity(r) == neg_identity)
                    .then_some(()))
            }
        },
    )
    .await
    .expect("catalog rediscovery must materialize the retained snapshots as discovered rows");

    // (c)+(e, CR side) retention converges: EXACTLY ONE config-labeled CR remains,
    // and it is an `origin=adopted` / Succeeded row — the adopted history, pruned to
    // keepLatest: 1. (Before this branch there was no adoption: the matching rows
    // would remain immortal `discovered` entries and config-labeled would stay 0.)
    let survivor = wait_until(
        "adopted history converges to exactly one config-labeled Succeeded row",
        Duration::from_secs(420),
        poll_interval(),
        || {
            let client = client.clone();
            async move {
                let rows = config_labeled(&client, E2E_NAMESPACE, POLICY).await;
                if rows.len() != 1 {
                    return Ok(None);
                }
                let r = &rows[0];
                let terminal = serde_json::to_value(r)
                    .unwrap_or_default()
                    .pointer("/status/phase")
                    .and_then(|p| p.as_str())
                    == Some("Succeeded")
                    && r.metadata.deletion_timestamp.is_none();
                Ok(terminal.then(|| r.clone()))
            }
        },
    )
    .await
    .expect("GFS retention over the ADOPTED rows must converge to exactly one config-labeled CR");
    assert_eq!(
        survivor.labels().get(ORIGIN_LABEL).map(String::as_str),
        Some("adopted"),
        "the surviving config-labeled row must carry the `origin: adopted` LABEL: {}",
        survivor.name_any()
    );
    assert_eq!(
        status_origin(&survivor),
        "adopted",
        "the surviving row must carry `status.origin: adopted`: {}",
        survivor.name_any()
    );

    // (d) the policy recorded the adoption wave: lastAdoptedCount ≥ 3 + the event.
    wait_until(
        "policy records status.adoption.lastAdoptedCount ≥ 3",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&policies, POLICY).await;
            Ok(s.pointer("/adoption/lastAdoptedCount")
                .and_then(|v| v.as_i64())
                .filter(|&n| n >= 3)
                .map(|_| ()))
        },
    )
    .await
    .expect("status.adoption.lastAdoptedCount must record the ≥3 adopted snapshots");
    assert!(
        event_exists(
            &client,
            E2E_NAMESPACE,
            SNAPSHOTS_ADOPTED_REASON,
            "SnapshotPolicy",
            POLICY
        )
        .await,
        "a `SnapshotsAdopted` Normal event must be published on the recreated policy"
    );

    // (e, kopia side) THE #210 ASSERTION: kopia history is pruned to exactly the
    // policy identity's kept snapshot (1) PLUS the untouched negative control (1) —
    // total 2. Before this branch adoption did not exist, so the ≥3 matching kopia
    // snapshots would remain forever (kopia count would stay at `kopia_before` ≥ 4);
    // that this dropped to 2 proves the adopted rows are now GFS-governed and their
    // kopia data was really deleted. (config-labeled == 1 above ⇒ the pruned adopted
    // CRs' finalizers already released ⇒ their kopia snapshot-deletes completed.)
    let kopia_final = observed_snapshot_count(&client, "e2e-adopt-verify-3", S1_SUBPATH).await;
    assert_eq!(
        kopia_final, 2,
        "kopia must hold exactly 1 (policy-identity, pruned from {produced_count}) + 1 (negative \
         control) = 2 snapshots after adoption+retention; got {kopia_final} (was {kopia_before})"
    );
    assert!(
        (kopia_final as usize) < kopia_before as usize,
        "the adopted history MUST have been pruned kopia-side (the #210 fix): before={kopia_before} after={kopia_final}"
    );

    // The negative control endures: still a forced-Retain `discovered` row, and its
    // kopia snapshot is one of the surviving 2.
    let disc = discovered_rows(&client, E2E_NAMESPACE, &repo_uid).await;
    let neg_rows: Vec<&Snapshot> = disc
        .iter()
        .filter(|r| row_identity(r) == neg_identity)
        .collect();
    assert_eq!(
        neg_rows.len(),
        1,
        "the non-matching seeded snapshot must remain a single `discovered` row: {neg_identity}"
    );
    let neg = neg_rows[0];
    assert_eq!(
        serde_json::to_value(neg)
            .unwrap_or_default()
            .pointer("/spec/deletionPolicy")
            .and_then(|x| x.as_str()),
        Some("Retain"),
        "the negative-control discovered row must be FORCED deletionPolicy Retain"
    );
    assert_eq!(
        status_origin(neg),
        "discovered",
        "the negative-control row must stay `discovered` (never adopted by a non-matching identity)"
    );

    // Cleanup: discovered rows are forced-Retain (deleting them leaves kopia intact);
    // remove Snapshot CRs BEFORE the Repository (finalizers need it).
    for r in config_labeled(&client, E2E_NAMESPACE, POLICY).await {
        let _ = backups
            .delete(&r.name_any(), &DeleteParams::default())
            .await;
    }
    for r in discovered_rows(&client, E2E_NAMESPACE, &repo_uid).await {
        let _ = backups
            .delete(&r.name_any(), &DeleteParams::default())
            .await;
    }
    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    let _ = wait_until(
        "snapshot CRs drain before repo teardown",
        Duration::from_secs(120),
        poll_interval(),
        || async {
            let live = backups
                .list(&ListParams::default().labels(&format!("{REPOSITORY_UID_LABEL}={repo_uid}")))
                .await?
                .items;
            Ok(live.is_empty().then_some(()))
        },
    )
    .await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}

// --- Scenario 2: the adoption opt-out keeps matching rows discovered --------------

const S2_SUBPATH: &str = "adopt-ignore";

/// With `catalog.adoption: Ignore` on the repository, a discovered snapshot whose
/// identity matches a live policy is LEFT ALONE: after the same delete→recreate flow
/// it re-materializes as a `discovered` row and STAYS discovered — no adopted rows,
/// no repeated on-demand scan-request stamping (the annotation is never stamped by an
/// inert adoption pass), and the policy's `status.adoption` records no adoptions.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn adoption_opt_out_stays_discovered() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client: Client = world.client().clone();
    ensure_repo(&client, S2_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-adopt-ignore-repo";
    const POLICY: &str = "e2e-adopt-ignore-pol";
    const SNAP: &str = "e2e-adopt-ignore-1";

    // The opt-out lives on the repository (`catalog.adoption: Ignore`); a fast refresh
    // so rediscovery is prompt; maintenance off.
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                S2_SUBPATH,
                serde_json::json!({
                    "maintenance": { "enabled": false },
                    "catalog": { "adoption": "Ignore", "periodicRefresh": true, "refreshInterval": "30s" }
                }),
            )),
        )
        .await
        .expect("create Repository with catalog.adoption: Ignore");
    wait_phase(&repos, REPO, "Ready")
        .await
        .expect("Repository should reach Ready");

    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                POLICY,
                "Repository",
                REPO,
                serde_json::json!({ "defaultDeletionPolicy": "Delete", "retention": { "keepLatest": 20 } }),
            )),
        )
        .await
        .expect("create SnapshotPolicy");

    // One matching snapshot, then delete the policy → the CR cascades (Retain) →
    // kopia retained → rediscovered.
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                SNAP,
                POLICY,
                serde_json::json!({ "deletionPolicy": "Delete" }),
            )),
        )
        .await
        .expect("create Snapshot");
    wait_phase(&backups, SNAP, "Succeeded")
        .await
        .expect("Snapshot should Succeed");
    let policy_identity = row_identity(&backups.get(SNAP).await.expect("get Snapshot"));

    policies
        .delete(POLICY, &DeleteParams::default())
        .await
        .expect("delete the SnapshotPolicy");
    wait_until(
        "the policy + its config-labeled CR drain",
        Duration::from_secs(240),
        poll_interval(),
        || async {
            let children = config_labeled(&client, E2E_NAMESPACE, POLICY).await;
            let gone = policies.get_opt(POLICY).await?.is_none();
            Ok((children.is_empty() && gone).then_some(()))
        },
    )
    .await
    .expect("the policy and its config-labeled CR must drain");

    let repo_uid = repos
        .get(REPO)
        .await
        .expect("get Repository")
        .uid()
        .expect("Repository uid");

    // Recreate the policy with the SAME identity. With repo `adoption: Ignore` and no
    // per-policy override, the effective adoption is Ignore → the pass is inert.
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                POLICY,
                "Repository",
                REPO,
                serde_json::json!({ "defaultDeletionPolicy": "Delete", "retention": { "keepLatest": 20 } }),
            )),
        )
        .await
        .expect("recreate the SnapshotPolicy");

    // The matching snapshot re-materializes as a discovered row (via periodicRefresh).
    wait_until(
        "the matching snapshot re-materializes as a discovered row",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            let repo_uid = repo_uid.clone();
            let policy_identity = policy_identity.clone();
            async move {
                let rows = discovered_rows(&client, E2E_NAMESPACE, &repo_uid).await;
                Ok(rows
                    .iter()
                    .any(|r| row_identity(r) == policy_identity)
                    .then_some(()))
            }
        },
    )
    .await
    .expect("the retained snapshot must re-materialize as a discovered row");

    // Settle window: for ~60s the matching row STAYS discovered — never adopted, no
    // adopted rows appear, the scan-request annotation is never stamped, and
    // status.adoption records no adoptions. Poll the invariants throughout.
    let settle_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        // No adopted rows for this repo, ever.
        let adopted: Vec<Snapshot> = {
            let api: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
            api.list(&ListParams::default().labels(&format!(
                "{ORIGIN_LABEL}=adopted,{REPOSITORY_UID_LABEL}={repo_uid}"
            )))
            .await
            .map(|l| l.items)
            .unwrap_or_default()
        };
        assert!(
            adopted.is_empty(),
            "adoption is opted OUT — no `origin: adopted` rows may ever appear: {:?}",
            adopted.iter().map(|a| a.name_any()).collect::<Vec<_>>()
        );
        // The matching row is still present AND still discovered.
        let disc = discovered_rows(&client, E2E_NAMESPACE, &repo_uid).await;
        assert!(
            disc.iter().any(|r| row_identity(r) == policy_identity),
            "the matching row must STAY a discovered row under the opt-out"
        );
        // The adoption pass never stamps an on-demand scan request in Ignore mode.
        assert!(
            scan_requested_annotation(&repos, REPO).await.is_none(),
            "an Ignore-mode adoption pass must NOT stamp `catalog-scan-requested-at`"
        );
        if Instant::now() >= settle_deadline {
            break;
        }
        tokio::time::sleep(poll_interval()).await;
    }

    // status.adoption records no adoptions (never written by an inert pass).
    let s = status_json(&policies, POLICY).await;
    assert!(
        s.pointer("/adoption/lastAdoptedCount")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            == 0,
        "status.adoption must record no adoptions under the opt-out; got {s}"
    );

    // Cleanup.
    for r in discovered_rows(&client, E2E_NAMESPACE, &repo_uid).await {
        let _ = backups
            .delete(&r.name_any(), &DeleteParams::default())
            .await;
    }
    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}

// --- Scenario 3: cross-namespace ClusterRepository adoption -----------------------

const S3_BUCKET: &str = "kopiur-adopt-crepo";

/// Run a foreign kopia seeder to completion.
async fn run_seeder(client: &Client, name: &str, steps: &[SeedStep<'_>]) {
    kopiur_e2e::apply::run_foreign_seeder(client, E2E_NAMESPACE, name, steps)
        .await
        .expect("foreign kopia seeder");
}

/// CROSS-NAMESPACE adoption for a `ClusterRepository` — the cluster-wide-LIST guard.
/// A foreign snapshot's identity hostname places its `discovered` row in the WORKLOAD
/// namespace, while the adopting `SnapshotPolicy` lives in the OPERATOR namespace (a
/// hostname override matches the foreign identity). Adoption must find the discovered
/// row cluster-wide and create the adopted row in the POLICY's namespace — a
/// policy-namespaced LIST would miss it. After a delete→recreate the adoption
/// re-converges cross-namespace.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn cluster_repository_adoption_cross_namespace() {
    let Some(world) = World::connect().await else {
        return;
    };
    // WorkloadNs implies Minio; Filesystem gives the operator-namespace source PVC the
    // policy references (its sourcePath is overridden, so only the name is needed).
    world
        .ensure(&[Need::WorkloadNs, Need::Filesystem])
        .await
        .expect("provision MinIO + workload namespace + operator source PVC");
    let client: Client = world.client().clone();

    let crepos: Api<ClusterRepository> = Api::all(client.clone());
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const CREPO: &str = "e2e-adopt-crepo";
    const POLICY: &str = "app"; // username default = policy name = "app"

    // Foreign writer: one snapshot under identity `app@<workload-ns>:/data/app`. The
    // hostname is the workload namespace, so the ClusterRepository places its
    // discovered row THERE.
    run_seeder(
        &client,
        "e2e-adopt-crepo-seed",
        &[
            SeedStep::WipeBucket { bucket: S3_BUCKET },
            SeedStep::WriteFile {
                dir: "app",
                file: "f.txt",
                content: "cross-ns-adopt",
            },
            SeedStep::CreateRepo {
                bucket: S3_BUCKET,
                username: "app",
                hostname: consts::WORKLOAD_NS,
            },
            SeedStep::Snapshot { dir: "app" },
        ],
    )
    .await;

    // Adopt-only ClusterRepository, open to all namespaces (so the OPERATOR-namespace
    // policy is tenancy-allowed AND the WORKLOAD-namespace placement is allowed), fast
    // refresh, maintenance off.
    crepos
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "ClusterRepository",
                "metadata": { "name": CREPO },
                "spec": {
                    "backend": { "s3": {
                        "bucket": S3_BUCKET,
                        "endpoint": consts::MINIO_ENDPOINT,
                        "region": "us-east-1",
                        "tls": { "disableTls": true },
                        "auth": { "secretRef": {
                            "name": consts::SECRET_S3_CREDS, "namespace": E2E_NAMESPACE
                        } }
                    }},
                    "encryption": { "passwordSecretRef": {
                        "name": consts::SECRET_S3_CREDS,
                        "namespace": E2E_NAMESPACE,
                        "key": "KOPIA_PASSWORD"
                    }},
                    "create": { "enabled": false },
                    "maintenance": { "enabled": false },
                    "allowedNamespaces": { "all": true },
                    "catalog": { "periodicRefresh": true, "refreshInterval": "30s" }
                }
            })),
        )
        .await
        .expect("create adopting ClusterRepository");
    wait_phase(&crepos, CREPO, "Ready")
        .await
        .expect("adopting ClusterRepository should reach Ready");

    let crepo_uid = crepos
        .get(CREPO)
        .await
        .expect("get ClusterRepository")
        .uid()
        .expect("ClusterRepository uid");
    let want_identity = format!("app@{}:/data/app", consts::WORKLOAD_NS);

    // The discovered row lands in the WORKLOAD namespace (identity hostname).
    wait_until(
        "the foreign snapshot is discovered in the WORKLOAD namespace",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            let crepo_uid = crepo_uid.clone();
            let want = want_identity.clone();
            async move {
                let rows = discovered_rows(&client, consts::WORKLOAD_NS, &crepo_uid).await;
                Ok(rows.iter().any(|r| row_identity(r) == want).then_some(()))
            }
        },
    )
    .await
    .expect("the discovered row must be placed in the workload namespace by identity hostname");

    // The adopting policy lives in the OPERATOR namespace, but a hostname override +
    // sourcePathOverride make its resolved identity EXACTLY the foreign one — so it
    // adopts the WORKLOAD-namespace row cross-namespace (the cluster-wide-LIST fix).
    let policy_json = |name: &str| -> serde_json::Value {
        snapshot_policy_json(
            E2E_NAMESPACE,
            name,
            "ClusterRepository",
            CREPO,
            serde_json::json!({
                "identity": { "hostname": consts::WORKLOAD_NS },
                "sources": [ { "pvc": { "name": consts::PVC_SRC }, "sourcePathOverride": "/data/app" } ],
                "retention": { "keepLatest": 20 }
            }),
        )
    };

    // Assert cross-namespace adoption converges: an `origin=adopted` row appears in the
    // POLICY's (operator) namespace with the foreign identity, twice (create, then
    // delete→recreate).
    let assert_adopts = |round: &'static str| {
        let client = client.clone();
        let want = want_identity.clone();
        async move {
            wait_until(
                &format!("[{round}] adopted row appears in the POLICY (operator) namespace"),
                Duration::from_secs(420),
                poll_interval(),
                || {
                    let client = client.clone();
                    let want = want.clone();
                    async move {
                        let rows = config_labeled(&client, E2E_NAMESPACE, POLICY).await;
                        let adopted = rows.iter().find(|r| {
                            r.labels().get(ORIGIN_LABEL).map(String::as_str) == Some("adopted")
                                && status_origin(r) == "adopted"
                                && row_identity(r) == want
                        });
                        Ok(adopted.cloned())
                    }
                },
            )
            .await
            .unwrap_or_else(|e| panic!("[{round}] cross-namespace adoption must converge: {e}"))
        }
    };

    policies
        .create(&PostParams::default(), &cr(policy_json(POLICY)))
        .await
        .expect("create the adopting SnapshotPolicy in the operator namespace");
    let first = assert_adopts("create").await;
    assert_eq!(
        first.namespace().as_deref(),
        Some(E2E_NAMESPACE),
        "the adopted row must live in the POLICY's namespace ({E2E_NAMESPACE}), not the discovered row's"
    );

    // Delete the policy (Retain cascade) → the adopted row is released, kopia retained,
    // rediscovered (back in the workload namespace). Recreate → adoption re-converges.
    policies
        .delete(POLICY, &DeleteParams::default())
        .await
        .expect("delete the adopting SnapshotPolicy");
    wait_until(
        "the policy + its adopted child drain",
        Duration::from_secs(240),
        poll_interval(),
        || async {
            let children = config_labeled(&client, E2E_NAMESPACE, POLICY).await;
            let gone = policies.get_opt(POLICY).await?.is_none();
            Ok((children.is_empty() && gone).then_some(()))
        },
    )
    .await
    .expect("the policy and its adopted child must drain");

    policies
        .create(&PostParams::default(), &cr(policy_json(POLICY)))
        .await
        .expect("recreate the adopting SnapshotPolicy");
    let second = assert_adopts("recreate").await;
    assert_eq!(
        second.namespace().as_deref(),
        Some(E2E_NAMESPACE),
        "after delete→recreate, cross-namespace adoption must again land the adopted row in the policy namespace"
    );

    // Cleanup (best-effort; cluster-scoped objects persist across reused clusters).
    for r in config_labeled(&client, E2E_NAMESPACE, POLICY).await {
        let _ = Api::<Snapshot>::namespaced(client.clone(), E2E_NAMESPACE)
            .delete(&r.name_any(), &DeleteParams::default())
            .await;
    }
    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    let _ = crepos.delete(CREPO, &DeleteParams::default()).await;
}

// --- Scenario 4: Retain-policy adoption converges without Job churn ---------------

const S4_BUCKET: &str = "kopiur-adopt-retain";
const S4_DIR: &str = "retain";

/// THE adopt/prune/rediscover-livelock regression pin. With an effective
/// `deletionPolicy: Retain` and a tight `retention: {keepLatest: 1}` over ≥3
/// pre-existing matching kopia snapshots, pre-fix code adopted ALL of them,
/// retention pruned the over-bound CRs (kopia untouched), the catalog
/// re-discovered them, adoption re-adopted them and stamped a fresh scan-request
/// token every wave — recreating the repository's discovery Job seconds after
/// each completion, forever, with discovered rows created and deleted every
/// cycle.
///
/// Fixture choices are load-bearing:
/// - an OBJECT-STORE repository, because a bare-path filesystem repo scans
///   in-process and has no discovery Job to churn;
/// - `periodicRefresh` OFF (the default), so the scan-request token arm is the
///   ONLY thing that can recycle the finished discovery Job — a stable Job uid
///   is then exactly the "no more tokens" proof. The natural token flow (the
///   adopt wave requests one drain scan) drives every scan this test needs.
///
/// Post-fix contract: adoption takes ONLY the GFS-kept newest snapshot
/// (invariant 8), the two over-bound ones stay `discovered`
/// (`status.adoption.skippedByRetention == 2` + an `AdoptionSkippedByRetention`
/// event), and after convergence the discovery Job uid, the scan-request
/// annotation, the discovered-row set, and the config-labeled set are all
/// byte-stable across a settle window. kopia data is never touched (Retain).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + MinIO + built images + helm install"]
async fn retain_policy_adoption_converges_without_job_churn() {
    let Some(world) = World::connect().await else {
        return;
    };
    // Minio for the object-store repo; Filesystem for the operator-namespace
    // source PVC the policy references (identity resolution only — no backup runs).
    world
        .ensure(&[Need::Filesystem, Need::Minio])
        .await
        .expect("provision MinIO + operator source PVC");
    let client: Client = world.client().clone();

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<k8s_openapi::api::batch::v1::Job> =
        Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-adopt-retain-repo";
    // The policy name doubles as the seeded kopia username (the resolver default).
    const POLICY: &str = "e2e-adopt-retain-pol";
    let job_name = format!("{REPO}-discovery");

    // Foreign history: THREE distinct snapshots under EXACTLY the identity the
    // policy below resolves to (username = policy name, hostname pinned, path =
    // the sourcePathOverride).
    run_seeder(
        &client,
        "e2e-adopt-retain-seed",
        &[
            SeedStep::WipeBucket { bucket: S4_BUCKET },
            SeedStep::WriteFile {
                dir: S4_DIR,
                file: "f.txt",
                content: "v1",
            },
            SeedStep::CreateRepo {
                bucket: S4_BUCKET,
                username: POLICY,
                hostname: E2E_NAMESPACE,
            },
            SeedStep::Snapshot { dir: S4_DIR },
            SeedStep::WriteFile {
                dir: S4_DIR,
                file: "f.txt",
                content: "v2",
            },
            SeedStep::Snapshot { dir: S4_DIR },
            SeedStep::WriteFile {
                dir: S4_DIR,
                file: "f.txt",
                content: "v3",
            },
            SeedStep::Snapshot { dir: S4_DIR },
        ],
    )
    .await;

    // Object-store repository, connect-only, maintenance off, NO periodicRefresh.
    repos
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Repository",
                "metadata": { "name": REPO, "namespace": E2E_NAMESPACE },
                "spec": {
                    "backend": { "s3": {
                        "bucket": S4_BUCKET,
                        "endpoint": consts::MINIO_ENDPOINT,
                        "region": "us-east-1",
                        "tls": { "disableTls": true },
                        "auth": { "secretRef": {
                            "name": consts::SECRET_S3_CREDS, "namespace": E2E_NAMESPACE
                        } }
                    }},
                    "encryption": { "passwordSecretRef": {
                        "name": consts::SECRET_S3_CREDS, "key": "KOPIA_PASSWORD"
                    }},
                    "create": { "enabled": false },
                    "maintenance": { "enabled": false }
                }
            })),
        )
        .await
        .expect("create object-store Repository");
    wait_phase(&repos, REPO, "Ready")
        .await
        .expect("Repository should reach Ready");
    let repo_uid = repos
        .get(REPO)
        .await
        .expect("get Repository")
        .uid()
        .expect("Repository uid");
    let want_identity = format!("{POLICY}@{E2E_NAMESPACE}:/data/{S4_DIR}");

    // The initial bootstrap scan materializes all three as discovered rows.
    wait_until(
        "the seeded history materializes as 3 discovered rows",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            let repo_uid = repo_uid.clone();
            let want = want_identity.clone();
            async move {
                let rows = discovered_rows(&client, E2E_NAMESPACE, &repo_uid).await;
                let matching = rows.iter().filter(|r| row_identity(r) == want).count();
                Ok((matching >= 3).then_some(()))
            }
        },
    )
    .await
    .expect("the initial bootstrap scan must discover the 3 seeded snapshots");

    // THE PATHOLOGICAL INPUT: Retain + keepLatest:1 over 3 matching candidates.
    // On pre-fix code this loops forever.
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                POLICY,
                "Repository",
                REPO,
                serde_json::json!({
                    "identity": { "hostname": E2E_NAMESPACE },
                    "sources": [ { "pvc": { "name": consts::PVC_SRC },
                                   "sourcePathOverride": format!("/data/{S4_DIR}") } ],
                    "defaultDeletionPolicy": "Retain",
                    "retention": { "keepLatest": 1 }
                }),
            )),
        )
        .await
        .expect("create the Retain SnapshotPolicy");

    // Convergence: exactly ONE adopted config-labeled row (the GFS-kept newest),
    // the two over-bound candidates deliberately left discovered
    // (skippedByRetention == 2), and the adopt wave's one drain-scan token
    // retired (annotation == status.catalog.scanRequestHonored). None of this is
    // ever simultaneously true on pre-fix code (skippedByRetention does not
    // exist there), so a livelocked operator times out here.
    wait_until(
        "adoption converges: 1 adopted row, 2 skipped-by-retention, token retired",
        Duration::from_secs(420),
        poll_interval(),
        || {
            let client = client.clone();
            let repos = repos.clone();
            let policies = policies.clone();
            let repo_uid = repo_uid.clone();
            let want = want_identity.clone();
            async move {
                let labeled = config_labeled(&client, E2E_NAMESPACE, POLICY).await;
                let one_adopted = labeled.len() == 1
                    && labeled[0].labels().get(ORIGIN_LABEL).map(String::as_str) == Some("adopted")
                    && status_origin(&labeled[0]) == "adopted"
                    && labeled[0].metadata.deletion_timestamp.is_none();
                let skipped = status_json(&policies, POLICY)
                    .await
                    .pointer("/adoption/skippedByRetention")
                    .and_then(|v| v.as_i64());
                let disc = discovered_rows(&client, E2E_NAMESPACE, &repo_uid).await;
                let disc_matching = disc.iter().filter(|r| row_identity(r) == want).count();
                let annotation = scan_requested_annotation(&repos, REPO).await;
                let honored = status_json(&repos, REPO)
                    .await
                    .pointer("/catalog/scanRequestHonored")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let token_retired = annotation.is_some() && annotation == honored;
                Ok(
                    (one_adopted && skipped == Some(2) && disc_matching == 2 && token_retired)
                        .then_some(()),
                )
            }
        },
    )
    .await
    .expect(
        "retention-aware adoption must converge (1 adopted, 2 left discovered, scan token \
         retired) — a timeout here means the adopt/prune/rediscover livelock is back",
    );

    // Capture the converged fixed points.
    let frozen_annotation = scan_requested_annotation(&repos, REPO)
        .await
        .expect("scan-request annotation present after convergence");
    let frozen_discovered: BTreeSet<(String, String)> =
        discovered_rows(&client, E2E_NAMESPACE, &repo_uid)
            .await
            .iter()
            .map(|r| (r.name_any(), r.uid().unwrap_or_default()))
            .collect();
    let frozen_survivor = config_labeled(&client, E2E_NAMESPACE, POLICY).await[0]
        .uid()
        .expect("adopted row uid");
    // The finished discovery Job may already be TTL-reaped; what matters is that
    // no NEW uid ever appears once converged.
    let mut job_uids: BTreeSet<String> = BTreeSet::new();
    if let Ok(Some(job)) = jobs.get_opt(&job_name).await
        && let Some(uid) = job.uid()
    {
        job_uids.insert(uid);
    }

    // Settle window: everything above is a fixed point. On pre-fix code the Job
    // uid churns and the discovered set flaps within one adoption wave (~30s).
    let settle_deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(Some(job)) = jobs.get_opt(&job_name).await
            && let Some(uid) = job.uid()
        {
            job_uids.insert(uid);
        }
        assert!(
            job_uids.len() <= 1,
            "the discovery Job must NOT be recycled after convergence (the scan-request \
             token arm is its only trigger here) — distinct uids seen: {job_uids:?}"
        );
        let annotation = scan_requested_annotation(&repos, REPO).await;
        assert_eq!(
            annotation.as_deref(),
            Some(frozen_annotation.as_str()),
            "no adoption pass may stamp a fresh scan-request token after convergence"
        );
        let disc_now: BTreeSet<(String, String)> =
            discovered_rows(&client, E2E_NAMESPACE, &repo_uid)
                .await
                .iter()
                .map(|r| (r.name_any(), r.uid().unwrap_or_default()))
                .collect();
        assert_eq!(
            disc_now, frozen_discovered,
            "the discovered-row set must be byte-stable (same names AND uids) — churn here \
             is the create/delete loop the user saw"
        );
        let labeled = config_labeled(&client, E2E_NAMESPACE, POLICY).await;
        assert!(
            labeled.len() == 1 && labeled[0].uid().as_deref() == Some(frozen_survivor.as_str()),
            "exactly the SAME adopted row must survive retention across the window"
        );
        if Instant::now() >= settle_deadline {
            break;
        }
        tokio::time::sleep(poll_interval()).await;
    }

    // Retain touched no kopia data: the repository still counts all 3 snapshots.
    let s = status_json(&repos, REPO).await;
    assert_eq!(
        s.pointer("/storageStats/snapshotCount")
            .and_then(|v| v.as_i64()),
        Some(3),
        "Retain adoption must never delete kopia data; repo status: {s}"
    );
    // The wave was recorded: one adoption + the skip event with the levers.
    assert_eq!(
        status_json(&policies, POLICY)
            .await
            .pointer("/adoption/lastAdoptedCount")
            .and_then(|v| v.as_i64()),
        Some(1),
        "exactly one snapshot (the GFS-kept newest) must have been adopted"
    );
    assert!(
        event_exists(
            &client,
            E2E_NAMESPACE,
            ADOPTION_SKIPPED_BY_RETENTION_REASON,
            "SnapshotPolicy",
            POLICY
        )
        .await,
        "an `AdoptionSkippedByRetention` Normal event must be published on the policy"
    );

    // Cleanup: Snapshot CRs BEFORE the Repository (finalizers need it).
    for r in config_labeled(&client, E2E_NAMESPACE, POLICY).await {
        let _ = backups
            .delete(&r.name_any(), &DeleteParams::default())
            .await;
    }
    for r in discovered_rows(&client, E2E_NAMESPACE, &repo_uid).await {
        let _ = backups
            .delete(&r.name_any(), &DeleteParams::default())
            .await;
    }
    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    let _ = wait_until(
        "snapshot CRs drain before repo teardown",
        Duration::from_secs(120),
        poll_interval(),
        || async {
            let live = backups
                .list(&ListParams::default().labels(&format!("{REPOSITORY_UID_LABEL}={repo_uid}")))
                .await?
                .items;
            Ok(live.is_empty().then_some(()))
        },
    )
    .await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}
