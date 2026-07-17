//! e2e: mass-deletion protection — the schedule-cascade guard and the
//! mass-deletion circuit breaker against a live operator (M4a wiring, M4b proof).
//!
//! Three scenarios prove the feature end-to-end and guard the ORIGINAL INCIDENT
//! (a schedule delete cascading into ~600 kopia snapshot deletions):
//!
//! 1. `schedule_cascade_delete_leaves_kopia_intact_and_recatalogs` — THE incident's
//!    regression guard. Deleting a `SnapshotSchedule` whose `onScheduleDelete` is the
//!    safe-default `Retain` removes the produced `Snapshot` CRs but keeps their kopia
//!    snapshots (no delete Jobs, unchanged kopia count), fires one
//!    `SnapshotRetainedOnScheduleDelete` Warning per CR, and the catalog rediscovers
//!    them as `origin: discovered` rows.
//! 2. `mass_deletion_breaker_holds_and_ack_drains` — a wave of external destructive
//!    deletions at/over the repository threshold is HELD (`DeletionHeld=True` /
//!    `MassDeletionBreaker`, repo `MassDeletionHeld=True`, no kopia data touched) until
//!    the `allow-mass-deletion` ack drains it (then the kopia snapshots are really
//!    deleted); a sub-threshold single delete afterwards is never held.
//! 3. `retention_prune_not_held_by_breaker` — operator GFS prunes bypass the breaker
//!    even at an aggressive threshold: the excess CRs (and their kopia snapshots) are
//!    pruned WITHOUT ever surfacing `DeletionHeld`.
//!
//! Deletion execution still uses the legacy per-CR `{name}-delete` Job (the batch
//! dispatcher is M5). These tests assert OUTCOMES (CRs drained, kopia counts, absence
//! of delete Jobs for a retained set, conditions/events) — never Job names as a
//! success signal — so they stay green through the M5 dispatcher swap.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; skip gracefully off-cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client, Resource, ResourceExt};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::events::v1::Event as EventsV1;

use kopiur_api::consts::{
    ALLOW_MASS_DELETION_ANNOTATION, MASS_DELETION_HELD_CONDITION, ORIGIN_LABEL,
    PRUNED_BY_ANNOTATION, REPOSITORY_UID_LABEL, SCHEDULE_LABEL,
};
use kopiur_api::{Repository, Snapshot, SnapshotPolicy, SnapshotSchedule};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

use common::{
    cr, ensure_repo, observed_snapshot_count, repository_json, snapshot_json, snapshot_policy_json,
    status_json, wait_condition, wait_phase,
};

// --- shared helpers ----------------------------------------------------------

/// True when a `Snapshot`'s live status carries `DeletionHeld=True` — the mark the
/// mass-deletion breaker leaves. Used both to assert a wave IS held and (negated) to
/// prove operator prunes are NEVER held.
fn snapshot_is_held(snap: &Snapshot) -> bool {
    let v = serde_json::to_value(snap).unwrap_or_default();
    v.pointer("/status/conditions")
        .and_then(|c| c.as_array())
        .is_some_and(|a| {
            a.iter().any(|c| {
                c.get("type").and_then(|t| t.as_str()) == Some("DeletionHeld")
                    && c.get("status").and_then(|s| s.as_str()) == Some("True")
            })
        })
}

/// Whether a `SnapshotRetainedOnScheduleDelete`-style Warning `Event` (by `reason`)
/// exists for the `Snapshot` named `snap_name`. The controller publishes
/// `events.k8s.io/v1` Events (kube-runtime 4.0 `Recorder`), whose involved object is
/// `regarding`. Matched by reason + regarding kind/name — the event outlives the
/// deleted CR, so it is still queryable after the finalizer released.
async fn snapshot_warning_event_exists(
    client: &Client,
    ns: &str,
    snap_name: &str,
    reason: &str,
) -> bool {
    let events: Api<EventsV1> = Api::namespaced(client.clone(), ns);
    let list = match events.list(&ListParams::default()).await {
        Ok(l) => l,
        Err(_) => return false,
    };
    list.items.iter().any(|e| {
        e.reason.as_deref() == Some(reason)
            && e.regarding.as_ref().is_some_and(|r| {
                r.kind.as_deref() == Some("Snapshot") && r.name.as_deref() == Some(snap_name)
            })
    })
}

/// The RFC3339 value the operator surfaced in a `DeletionHeld` message's ack command
/// (`…allow-mass-deletion="<value>"…`) — the newest pending `deletionTimestamp` for
/// the repository, to copy verbatim into the repository annotation.
fn parse_ack_value(msg: &str) -> Option<String> {
    let needle = format!("{ALLOW_MASS_DELETION_ANNOTATION}=\"");
    let start = msg.find(&needle)? + needle.len();
    let rest = &msg[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// A `SnapshotSchedule` firing `policy` on `cron` (with `runOnCreate`), default
/// deletion semantics (NO `spec.deletion` → `onScheduleDelete: Retain`).
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

// --- Scenario 1: schedule-cascade guard (the incident's regression guard) -----

const CASCADE_SUBPATH: &str = "massdel-cascade";

/// THE INCIDENT'S REGRESSION GUARD. A `SnapshotSchedule` produces several `Snapshot`
/// CRs (deletionPolicy `Delete`, stamped `onScheduleDelete: Retain` — the safe
/// default). Deleting the schedule GC-cascades those CRs; the finalizer MUST keep the
/// kopia snapshots (the incident deleted them). We assert the discriminating
/// mid-states the fix encodes — NO delete Jobs (b) and an UNCHANGED kopia count (c) —
/// because the pre-M4a counterfactual (which would have launched delete Jobs and
/// destroyed the snapshots) can't be run in CI. Also: the CRs drain (a), each fires a
/// `SnapshotRetainedOnScheduleDelete` Warning (d), and the catalog rediscovers the
/// retained snapshots as forced-Retain `origin: discovered` rows (e).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn schedule_cascade_delete_leaves_kopia_intact_and_recatalogs() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client: Client = world.client().clone();
    ensure_repo(&client, CASCADE_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-massdel-cascade-repo";
    const POLICY: &str = "e2e-massdel-cascade-pol";
    const SCHED: &str = "e2e-massdel-cascade-sched";

    // A fast catalog refresh so rediscovery (assertion e) doesn't wait an hour;
    // maintenance off to cut Job churn. No deletionProtection — the cascade guard is
    // owner-driven and orthogonal to the breaker (retained deletions don't count).
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                CASCADE_SUBPATH,
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

    // Produced snapshots default to deletionPolicy Delete (so the guard's
    // Retain-over-Delete downgrade is what fires); generous retention so no slot is
    // GFS-pruned mid-run.
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

    schedules
        .create(
            &PostParams::default(),
            &cr::<SnapshotSchedule>(schedule_json(SCHED, POLICY, "* * * * *")),
        )
        .await
        .expect("create SnapshotSchedule");

    let sched_selector = format!("{SCHEDULE_LABEL}={SCHED}");
    let succeeded_produced = || async {
        let list = backups
            .list(&ListParams::default().labels(&sched_selector))
            .await?;
        let ready: Vec<Snapshot> = list
            .items
            .into_iter()
            .filter(|b| {
                let v = serde_json::to_value(b).unwrap_or_default();
                v.pointer("/status/phase").and_then(|p| p.as_str()) == Some("Succeeded")
                    && v.pointer("/status/snapshot/kopiaSnapshotID")
                        .and_then(|s| s.as_str())
                        .is_some_and(|s| !s.is_empty())
            })
            .collect();
        anyhow::Ok(ready)
    };

    // >=2 produced Snapshots reach Succeeded with real kopia ids.
    wait_until(
        "schedule produces >=2 Succeeded snapshots with kopia ids",
        default_timeout(),
        poll_interval(),
        || async {
            let ready = succeeded_produced()
                .await
                .map_err(|e| kube::Error::Service(e.into()))?;
            Ok((ready.len() >= 2).then_some(()))
        },
    )
    .await
    .expect("the schedule should produce >=2 Succeeded snapshots");

    // Suspend so the produced set stops growing, then let any in-flight run settle so
    // the recorded set + kopia count are stable across the cascade.
    schedules
        .patch(
            SCHED,
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({ "spec": { "schedule": { "suspend": true } } })),
        )
        .await
        .expect("suspend schedule to freeze the produced set");
    tokio::time::sleep(Duration::from_secs(12)).await;

    let produced = succeeded_produced().await.expect("list produced snapshots");
    let produced_names: Vec<String> = produced.iter().map(|b| b.name_any()).collect();
    assert!(
        produced_names.len() >= 2,
        "need >=2 produced snapshots to prove a MASS cascade retains all; got {produced_names:?}"
    );
    // The discriminating shape the incident deleted: deletionPolicy Delete AND the
    // safe-default onScheduleDelete Retain stamped at creation.
    for b in &produced {
        let v = serde_json::to_value(b).unwrap_or_default();
        assert_eq!(
            v.pointer("/spec/deletionPolicy").and_then(|x| x.as_str()),
            Some("Delete"),
            "a produced scheduled snapshot must default to deletionPolicy Delete: {v}"
        );
        assert_eq!(
            v.pointer("/spec/onScheduleDelete").and_then(|x| x.as_str()),
            Some("Retain"),
            "a produced scheduled snapshot must be stamped onScheduleDelete Retain: {v}"
        );
    }

    let kopia_before =
        observed_snapshot_count(&client, "e2e-massdel-cascade-verify-1", CASCADE_SUBPATH).await;
    assert!(
        kopia_before >= 2,
        "kopia should hold >=2 snapshots before the cascade, got {kopia_before}"
    );

    // THE INCIDENT: delete the SnapshotSchedule. Kubernetes GC cascade-deletes the
    // produced Snapshot CRs; each finalizer must RETAIN (not delete) its kopia
    // snapshot because onScheduleDelete is the safe-default Retain.
    schedules
        .delete(SCHED, &DeleteParams::default())
        .await
        .expect("delete the SnapshotSchedule (the incident's trigger)");

    // (a) all produced CRs drain AND (b) ZERO delete Jobs are ever created for them.
    // A `{name}-delete` Job's lifetime is bounded by its CR's, so polling both in
    // lockstep across the whole drain window would catch any delete Job a pre-M4a
    // operator launched — the fingerprint of the fix. (We can't run the pre-M4a
    // counterfactual in CI; these mid-states encode it. — brief §1.)
    let drain_deadline = Instant::now() + Duration::from_secs(240);
    loop {
        for name in &produced_names {
            if jobs
                .get_opt(&format!("{name}-delete"))
                .await
                .expect("get delete Job")
                .is_some()
            {
                panic!(
                    "a `{name}-delete` Job was created for a cascade-RETAINED snapshot — this is \
                     the pre-M4a mass-deletion regression (the schedule delete would destroy the \
                     kopia snapshots)"
                );
            }
        }
        let remaining = backups
            .list(&ListParams::default().labels(&sched_selector))
            .await
            .expect("list produced snapshots")
            .items;
        if remaining.is_empty() {
            break;
        }
        assert!(
            Instant::now() < drain_deadline,
            "produced Snapshot CRs did not drain after the schedule delete: {:?}",
            remaining.iter().map(|b| b.name_any()).collect::<Vec<_>>()
        );
        tokio::time::sleep(poll_interval()).await;
    }

    // (c) kopia-side snapshot count UNCHANGED — the cascade retained everything.
    let kopia_after =
        observed_snapshot_count(&client, "e2e-massdel-cascade-verify-2", CASCADE_SUBPATH).await;
    assert_eq!(
        kopia_after, kopia_before,
        "kopia snapshot count MUST be unchanged by a Retain-cascade schedule delete \
         (the incident's data loss); before={kopia_before} after={kopia_after}"
    );

    // (d) each vanished CR got a `SnapshotRetainedOnScheduleDelete` Warning event.
    for name in &produced_names {
        assert!(
            snapshot_warning_event_exists(
                &client,
                E2E_NAMESPACE,
                name,
                "SnapshotRetainedOnScheduleDelete"
            )
            .await,
            "cascade-retained snapshot {name} must get a SnapshotRetainedOnScheduleDelete Warning"
        );
    }

    // (e) within the catalog interval, discovered rows re-materialize for the retained
    // kopia snapshots (keyed to THIS repo's uid), each forced deletionPolicy Retain.
    let repo_uid = repos
        .get(REPO)
        .await
        .expect("get Repository")
        .uid()
        .expect("Repository uid");
    let disc_selector = format!("{ORIGIN_LABEL}=discovered,{REPOSITORY_UID_LABEL}={repo_uid}");
    let discovered = wait_until(
        "catalog rediscovers the retained kopia snapshots as discovered rows",
        default_timeout(),
        poll_interval(),
        || async {
            let rows = backups
                .list(&ListParams::default().labels(&disc_selector))
                .await?
                .items;
            Ok((rows.len() as i64 >= kopia_before).then_some(rows))
        },
    )
    .await
    .expect("the retained kopia snapshots must re-materialize as discovered Snapshot CRs");
    for row in &discovered {
        let v = serde_json::to_value(row).unwrap_or_default();
        assert_eq!(
            v.pointer("/spec/deletionPolicy").and_then(|x| x.as_str()),
            Some("Retain"),
            "a rediscovered snapshot must be FORCED deletionPolicy Retain: {v}"
        );
    }

    // Cleanup: discovered rows are forced-Retain, so deleting them leaves kopia
    // intact; remove Snapshot CRs BEFORE the Repository (the finalizer needs it).
    for row in &discovered {
        let _ = backups
            .delete(&row.name_any(), &DeleteParams::default())
            .await;
    }
    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    // Best-effort: let the discovered CRs' finalizers release before the repo goes.
    let _ = wait_until(
        "discovered CRs drain before repo teardown",
        Duration::from_secs(60),
        poll_interval(),
        || async {
            let rows = backups
                .list(&ListParams::default().labels(&disc_selector))
                .await?
                .items;
            Ok(rows.is_empty().then_some(()))
        },
    )
    .await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}

// --- Scenario 2: mass-deletion breaker holds + ack drains ---------------------

const BREAKER_SUBPATH: &str = "massdel-breaker";

/// A bulk-delete of manual `Snapshot`s at/over the repository breaker threshold is
/// HELD (no kopia data touched), the ack drains the whole wave (and REALLY deletes the
/// kopia snapshots), and a later sub-threshold single delete is not held.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn mass_deletion_breaker_holds_and_ack_drains() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client: Client = world.client().clone();
    ensure_repo(&client, BREAKER_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-massdel-breaker-repo";
    const POLICY: &str = "e2e-massdel-breaker-pol";
    const NAMES: [&str; 3] = [
        "e2e-massdel-breaker-1",
        "e2e-massdel-breaker-2",
        "e2e-massdel-breaker-3",
    ];

    // threshold: 2 → 3 pending external destructive deletions trip the breaker. A fast
    // catalog refresh so the repo re-reconciles promptly and its (lazy) MassDeletionHeld
    // condition appears without a 5-minute wait; maintenance off to cut Job churn.
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                BREAKER_SUBPATH,
                serde_json::json!({
                    "deletionProtection": { "threshold": 2 },
                    "maintenance": { "enabled": false },
                    "catalog": { "periodicRefresh": true, "refreshInterval": "30s" }
                }),
            )),
        )
        .await
        .expect("create Repository with a threshold-2 breaker");
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
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create SnapshotPolicy");

    // Three manual snapshots (origin manual, deletionPolicy Delete), all Succeeded.
    for n in NAMES {
        backups
            .create(
                &PostParams::default(),
                &cr(snapshot_json(
                    E2E_NAMESPACE,
                    n,
                    POLICY,
                    serde_json::json!({ "deletionPolicy": "Delete" }),
                )),
            )
            .await
            .unwrap_or_else(|e| panic!("create manual Snapshot {n}: {e}"));
    }
    for n in NAMES {
        wait_phase(&backups, n, "Succeeded")
            .await
            .unwrap_or_else(|e| panic!("manual Snapshot {n} should reach Succeeded: {e}"));
    }
    let kopia_before =
        observed_snapshot_count(&client, "e2e-massdel-breaker-verify-1", BREAKER_SUBPATH).await;
    assert_eq!(
        kopia_before, 3,
        "the three manual snapshots must all exist in kopia before the wave, got {kopia_before}"
    );

    // Bulk-delete all three (no schedule involved — the owner-independent breaker path).
    for n in NAMES {
        backups
            .delete(n, &DeleteParams::default())
            .await
            .unwrap_or_else(|e| panic!("delete Snapshot {n}: {e}"));
    }

    // (a) all three stay terminating with DeletionHeld=True / MassDeletionBreaker.
    for n in NAMES {
        let cond = wait_condition(&backups, n, "DeletionHeld", "True")
            .await
            .unwrap_or_else(|e| panic!("{n} must be HELD by the mass-deletion breaker: {e}"));
        assert_eq!(
            cond.get("reason").and_then(|r| r.as_str()),
            Some("MassDeletionBreaker"),
            "{n} DeletionHeld reason must be MassDeletionBreaker: {cond}"
        );
    }

    // (b) the held message carries the ack annotation name AND an RFC3339 value — the
    // operator-surfaced newest-pending deletionTimestamp. Parse it out.
    let held_status = status_json(&backups, NAMES[0]).await;
    let held_msg = held_status
        .get("conditions")
        .and_then(|c| c.as_array())
        .and_then(|a| {
            a.iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("DeletionHeld"))
        })
        .and_then(|c| c.get("message").and_then(|m| m.as_str()))
        .unwrap_or_default()
        .to_string();
    assert!(
        held_msg.contains(ALLOW_MASS_DELETION_ANNOTATION),
        "the held message must name the ack annotation: {held_msg}"
    );
    let ack_value = parse_ack_value(&held_msg)
        .unwrap_or_else(|| panic!("the held message must carry an ack value: {held_msg}"));
    chrono::DateTime::parse_from_rfc3339(&ack_value)
        .unwrap_or_else(|_| panic!("the surfaced ack value must be RFC3339: {ack_value:?}"));

    // (c) the Repository gains MassDeletionHeld=True (lazy — on the repo's own cadence).
    wait_condition(&repos, REPO, MASS_DELETION_HELD_CONDITION, "True")
        .await
        .expect("Repository must surface MassDeletionHeld=True while the wave is held");

    // (d) kopia count UNCHANGED while held.
    let kopia_held =
        observed_snapshot_count(&client, "e2e-massdel-breaker-verify-2", BREAKER_SUBPATH).await;
    assert_eq!(
        kopia_held, kopia_before,
        "NO kopia data may be deleted while the wave is HELD; before={kopia_before} held={kopia_held}"
    );

    // (e) annotate the Repository with the EXACT parsed value → all three drain. The
    // 5-minute held requeue does NOT gate this: the repository-annotation watch
    // (repository_to_deleting_snapshots) re-enqueues the held CRs promptly, so we
    // assert the drain within ~2min of the ack (brief §runtime).
    repos
        .patch(
            REPO,
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({
                "metadata": { "annotations": { ALLOW_MASS_DELETION_ANNOTATION: ack_value } }
            })),
        )
        .await
        .expect("acknowledge the mass-deletion wave on the repository");
    for n in NAMES {
        wait_until(
            &format!("{n} drains after the ack"),
            Duration::from_secs(150),
            poll_interval(),
            || async { Ok(backups.get_opt(n).await?.is_none().then_some(())) },
        )
        .await
        .unwrap_or_else(|e| {
            panic!(
                "{n} must drain within ~2min of the ack (repo-annotation watch re-triggers): {e}"
            )
        });
    }
    // The delete REALLY happened: the kopia snapshots dropped by 3.
    let kopia_drained =
        observed_snapshot_count(&client, "e2e-massdel-breaker-verify-3", BREAKER_SUBPATH).await;
    assert_eq!(
        kopia_drained,
        kopia_before - 3,
        "the acked wave must actually delete all 3 kopia snapshots; before={kopia_before} after={kopia_drained}"
    );

    // (f) a NEW single manual snapshot delete (1 < threshold 2) proceeds WITHOUT
    // holding — a below-threshold + stale-ack-inert sanity in one (the stale ack from
    // (e) predates this snapshot, so it cannot be what lets it through).
    const SOLO: &str = "e2e-massdel-breaker-solo";
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                SOLO,
                POLICY,
                serde_json::json!({ "deletionPolicy": "Delete" }),
            )),
        )
        .await
        .expect("create the solo manual Snapshot");
    wait_phase(&backups, SOLO, "Succeeded")
        .await
        .expect("solo Snapshot should reach Succeeded");
    backups
        .delete(SOLO, &DeleteParams::default())
        .await
        .expect("delete the solo Snapshot");
    wait_until(
        "solo sub-threshold delete drains WITHOUT ever being held",
        Duration::from_secs(150),
        poll_interval(),
        || async {
            match backups.get_opt(SOLO).await? {
                Some(s) => {
                    assert!(
                        !snapshot_is_held(&s),
                        "a sub-threshold ({} < 2) single delete must NEVER be held by the breaker",
                        1
                    );
                    Ok(None)
                }
                None => Ok(Some(())),
            }
        },
    )
    .await
    .expect("the solo Snapshot should drain normally, never held");

    // Cleanup.
    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}

// --- Scenario 3: retention prune is not held by the breaker -------------------

const PRUNE_SUBPATH: &str = "massdel-prune";

/// Operator GFS prunes bypass the breaker even at an AGGRESSIVE threshold (1): a
/// schedule producing >=3 snapshots is pruned down to `keepLatest: 1` — the excess
/// CRs AND their kopia snapshots are removed — WITHOUT any of them ever surfacing
/// `DeletionHeld`. A pruned CR carries `pruned-by: retention` while terminating
/// (asserted best-effort — the terminating window is short).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn retention_prune_not_held_by_breaker() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client: Client = world.client().clone();
    ensure_repo(&client, PRUNE_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-massdel-prune-repo";
    const POLICY: &str = "e2e-massdel-prune-pol";
    const SCHED: &str = "e2e-massdel-prune-sched";

    // AGGRESSIVE breaker (threshold: 1) — every EXTERNAL destructive deletion would be
    // held. The point: operator retention prunes bypass the breaker, so GFS still
    // prunes. maintenance off to cut Job churn.
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                PRUNE_SUBPATH,
                serde_json::json!({
                    "deletionProtection": { "threshold": 1 },
                    "maintenance": { "enabled": false }
                }),
            )),
        )
        .await
        .expect("create Repository with a threshold-1 breaker");
    wait_phase(&repos, REPO, "Ready")
        .await
        .expect("Repository should reach Ready");

    // GFS keeps exactly 1; produced snapshots delete their kopia data on prune
    // (defaultDeletionPolicy Delete), so the prune is provably a real kopia deletion.
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
        .expect("create SnapshotPolicy keeping 1");

    schedules
        .create(
            &PostParams::default(),
            &cr::<SnapshotSchedule>(schedule_json(SCHED, POLICY, "* * * * *")),
        )
        .await
        .expect("create SnapshotSchedule");

    let sched_selector = format!("{SCHEDULE_LABEL}={SCHED}");
    // Collect >=3 distinct produced names across the prune window, asserting on every
    // poll that NONE ever carries DeletionHeld=True, and catching (best-effort) a
    // terminating pruned CR's `pruned-by: retention` annotation.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut saw_pruned_by_retention = false;
    let collect_deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let list = backups
            .list(&ListParams::default().labels(&sched_selector))
            .await
            .expect("list produced snapshots")
            .items;
        for b in &list {
            seen.insert(b.name_any());
            assert!(
                !snapshot_is_held(b),
                "a retention-pruned snapshot must NEVER be held by the mass-deletion breaker \
                 (operator prunes bypass it): {}",
                b.name_any()
            );
            if b.meta().deletion_timestamp.is_some()
                && let Some(pb) = b.annotations().get(PRUNED_BY_ANNOTATION)
            {
                assert_eq!(
                    pb, "retention",
                    "a GFS-pruned CR terminating must carry pruned-by: retention, got {pb:?}"
                );
                saw_pruned_by_retention = true;
            }
        }
        if seen.len() >= 3 {
            break;
        }
        assert!(
            Instant::now() < collect_deadline,
            "the schedule should produce >=3 snapshots over a few slots; saw {seen:?}"
        );
        tokio::time::sleep(poll_interval()).await;
    }

    // Suspend so no new snapshots are produced, then let GFS converge to keepLatest=1,
    // still asserting nothing is ever held during the convergence.
    schedules
        .patch(
            SCHED,
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({ "spec": { "schedule": { "suspend": true } } })),
        )
        .await
        .expect("suspend schedule");

    let converge_deadline = Instant::now() + Duration::from_secs(240);
    let live = loop {
        let list = backups
            .list(&ListParams::default().labels(&sched_selector))
            .await
            .expect("list produced snapshots")
            .items;
        for b in &list {
            assert!(
                !snapshot_is_held(b),
                "a retention-pruned snapshot must NEVER be held by the breaker (convergence): {}",
                b.name_any()
            );
            if b.meta().deletion_timestamp.is_some()
                && let Some(pb) = b.annotations().get(PRUNED_BY_ANNOTATION)
            {
                assert_eq!(pb, "retention", "pruned CR must carry pruned-by: retention");
                saw_pruned_by_retention = true;
            }
        }
        // Only terminal (non-terminating) survivors count toward the settled set.
        let settled: Vec<String> = list
            .iter()
            .filter(|b| b.meta().deletion_timestamp.is_none())
            .map(|b| b.name_any())
            .collect();
        if settled.len() <= 1 && list.len() <= 1 {
            break settled;
        }
        assert!(
            Instant::now() < converge_deadline,
            "GFS retention should prune to keepLatest=1; still {} live: {:?}",
            list.len(),
            list.iter().map(|b| b.name_any()).collect::<Vec<_>>()
        );
        tokio::time::sleep(poll_interval()).await;
    };

    // The prune deleted kopia data too: >=3 produced, but only the kept set survives
    // kopia-side (== the live CR count), which is strictly fewer than produced.
    let kopia_after =
        observed_snapshot_count(&client, "e2e-massdel-prune-verify", PRUNE_SUBPATH).await;
    assert_eq!(
        kopia_after,
        live.len() as i64,
        "kopia must hold exactly the kept snapshots after the prune; live CRs={} kopia={kopia_after}",
        live.len()
    );
    assert!(
        (kopia_after as usize) < seen.len(),
        "the prune must have DELETED kopia snapshots: produced {} distinct, kopia now {kopia_after}",
        seen.len()
    );
    // Best-effort signal (never asserted as required — the terminating window is short).
    if saw_pruned_by_retention {
        eprintln!("observed a terminating GFS-pruned CR carrying pruned-by: retention");
    }

    // Cleanup: remaining Snapshot CRs BEFORE the Repository (finalizers need it).
    for b in backups
        .list(&ListParams::default().labels(&sched_selector))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
    {
        let _ = backups
            .delete(&b.name_any(), &DeleteParams::default())
            .await;
    }
    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}
