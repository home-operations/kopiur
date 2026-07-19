//! e2e: mass-deletion protection — the schedule-cascade guard, the mass-deletion
//! circuit breaker, AND the per-repository BATCH delete dispatcher against a live
//! operator (M4a wiring, M4b proof, M5b batch-dispatcher proof).
//!
//! Scenarios 1-3 prove the guard/breaker end-to-end and guard the ORIGINAL INCIDENT
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
//! Scenarios 5-8 (M5b) prove the M5a per-repository BATCH delete dispatcher, which
//! replaced the per-CR `{name}-delete` Job with one repository-scoped
//! `SnapshotDeleteBatch` mover Job per accumulation window. These LIST batch Jobs by
//! the op label + managed-by and read the `delete-members` UID annotation to prove the
//! wire behavior (batch-of-1 unification, no-overlap concurrency, the concurrency
//! throttle, and outage retry) — not just the drained OUTCOME scenarios 1-3 assert:
//!
//! 5. `single_snapshot_delete_still_releases` — one manual delete makes exactly one
//!    batch Job whose members are exactly that CR's UID; the CR drains, kopia -1.
//! 6. `concurrent_batches_do_not_overlap` — two disjoint delete waves make two
//!    disjoint batch Jobs (uncapped default allows concurrency); all drain, kopia -5.
//! 7. `throttle_caps_concurrent_batch_jobs` — with `KOPIUR_MAX_CONCURRENT_DELETE_JOBS=1`
//!    on the operator Deployment, at most one live batch Job exists at any instant;
//!    both waves still drain. Restores the env (rollout wait) even on failure.
//! 8. `batch_delete_retries_after_repo_outage` — with the repo backend broken (its dir
//!    flipped read-only), the batch Job fails, is reaped, and refires (≥2 distinct Job
//!    generations) while NO finalizer releases; restoring writability drains all + the
//!    kopia snapshots are really deleted. Fail-safe under outage + convergence after.
//!
//! Scenarios 1-3 assert OUTCOMES (CRs drained, kopia counts, conditions/events); 5-8
//! additionally assert the batch Job wire shape (op label, `delete-members`).
//!
//! Scenario 9 (the final-review flagship counterexample) proves the count-vs-fire
//! polarity: a breaker-exempt operator PRUNE firing on a repository whose external wave
//! is HELD must NOT sweep the held members into its own batch:
//!
//! 9. `held_wave_not_swept_by_concurrent_prune` — repo threshold 2, an aggressive
//!    (`keepLatest: 1`) scheduled policy pruning continuously, and 3 HELD manual
//!    externals on the SAME repo. While GFS prunes ≥2 scheduled snapshots (real batch
//!    Jobs firing on the repo), the 3 held manuals must stay terminating + never drain,
//!    then drain (kopia -3) only once acked — the fire set excludes held externals even
//!    when a prune's batch is running.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; skip gracefully off-cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client, Resource, ResourceExt};

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::events::v1::Event as EventsV1;

use kopiur_api::consts::{
    ALLOW_MASS_DELETION_ANNOTATION, CONFIG_LABEL, MANAGED_BY_LABEL, MANAGED_BY_VALUE,
    MASS_DELETION_HELD_CONDITION, OP_LABEL, ORIGIN_LABEL, PRUNED_BY_ANNOTATION,
    REPOSITORY_UID_LABEL, SCHEDULE_LABEL, SNAPSHOT_CLEANUP_FINALIZER,
};
use kopiur_api::{Repository, Snapshot, SnapshotPolicy, SnapshotSchedule};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, builders, consts as e2e_consts, default_timeout, poll_interval,
    wait, wait_until,
};

use common::{
    cr, ensure_repo, observed_snapshot_count, repository_json, snapshot_json, snapshot_policy_json,
    status_json, wait_condition, wait_phase,
};

// --- Batch-dispatcher wire contract (M5a) -------------------------------------
// The batch-delete Job's op-label VALUE and its member-UID annotation are defined in
// the controller crate (`crate::consts::{OP_SNAPSHOT_DELETE_BATCH, DELETE_MEMBERS_ANNOTATION}`),
// which the e2e crate does not depend on. Mirror them here as DELIBERATE literals — the
// same pattern `common::WORK_SPEC_ENV` uses for the controller↔mover work-spec env: an
// accidental rename in the controller must fail THIS suite at runtime (the label/annotation
// are a wire contract these scenarios read). The label KEY (`OP_LABEL`), `MANAGED_BY_*`, and
// `SNAPSHOT_CLEANUP_FINALIZER` are shared via `kopiur_api::consts` and imported above.
const OP_SNAPSHOT_DELETE_BATCH: &str = "snapshot-delete-batch";
const DELETE_MEMBERS_ANNOTATION: &str = "kopiur.home-operations.com/delete-members";
/// The operator controller Deployment the chart installs (`<release>-controller`).
const CONTROLLER_DEPLOYMENT: &str = "kopiur-controller";
/// The controller container name in that Deployment (`deploy/helm/kopiur/templates/deployment.tpl`).
const CONTROLLER_CONTAINER: &str = "controller";
/// Env knob capping concurrent batch-delete Jobs (`crate::config::MAX_CONCURRENT_DELETE_JOBS_ENV`).
const MAX_CONCURRENT_DELETE_JOBS_ENV: &str = "KOPIUR_MAX_CONCURRENT_DELETE_JOBS";

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

// --- Batch-dispatcher helpers (scenarios 5-8) ---------------------------------

/// Terminal (or not) state of a batch delete Job, mirroring the controller's
/// `job_terminal_state` (a `Complete`/`Failed`=True condition, else the succeeded
/// count) so the tests classify a Job exactly as the dispatcher does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchJobState {
    Live,
    Succeeded,
    Failed,
}

fn batch_job_state(job: &Job) -> BatchJobState {
    let status = job.status.as_ref();
    if let Some(conds) = status.and_then(|s| s.conditions.as_ref()) {
        for c in conds {
            if c.status == "True" && c.type_ == "Complete" {
                return BatchJobState::Succeeded;
            }
            if c.status == "True" && c.type_ == "Failed" {
                return BatchJobState::Failed;
            }
        }
    }
    if status.and_then(|s| s.succeeded).unwrap_or(0) >= 1 {
        return BatchJobState::Succeeded;
    }
    BatchJobState::Live
}

/// The member `Snapshot` UIDs a batch Job covers, from its comma-joined
/// `delete-members` annotation (the dispatcher's single source of truth).
fn batch_members(job: &Job) -> BTreeSet<String> {
    job.annotations()
        .get(DELETE_MEMBERS_ANNOTATION)
        .map(|v| {
            v.split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// LIST every kopiur-managed batch delete Job in the operator namespace (op label +
/// managed-by). Scenarios narrow to "their" repository by member-UID intersection
/// (each scenario has an isolated repo + its own snapshots), which is equivalent to
/// the dispatcher's repo-hash label filter without reproducing the internal hash.
async fn list_batch_jobs(jobs: &Api<Job>) -> Vec<Job> {
    let selector =
        format!("{MANAGED_BY_LABEL}={MANAGED_BY_VALUE},{OP_LABEL}={OP_SNAPSHOT_DELETE_BATCH}");
    jobs.list(&ListParams::default().labels(&selector))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

/// The batch Jobs whose members are all within `mine` (this scenario's snapshot
/// UIDs) — i.e. this repository's batch Jobs. A Job with any foreign member is
/// skipped (belongs to another scenario), so a leftover from an earlier scenario
/// can never be miscounted here.
async fn my_batch_jobs(jobs: &Api<Job>, mine: &BTreeSet<String>) -> Vec<Job> {
    list_batch_jobs(jobs)
        .await
        .into_iter()
        .filter(|j| {
            let m = batch_members(j);
            !m.is_empty() && m.is_subset(mine)
        })
        .collect()
}

/// Poll until a kopiur batch Job whose members are EXACTLY `target` appears,
/// returning it. Result-returning so a must-restore scenario can `?`-propagate.
async fn find_batch_with_members(
    jobs: &Api<Job>,
    target: &BTreeSet<String>,
) -> anyhow::Result<Job> {
    wait_until(
        &format!("batch Job with members {target:?} appears"),
        default_timeout(),
        poll_interval(),
        || {
            let jobs = jobs.clone();
            let target = target.clone();
            async move {
                let js = list_batch_jobs(&jobs).await;
                Ok(js.into_iter().find(|j| batch_members(j) == target))
            }
        },
    )
    .await
}

/// Panicking wrapper over [`find_batch_with_members`].
async fn wait_batch_with_members(jobs: &Api<Job>, target: &BTreeSet<String>) -> Job {
    find_batch_with_members(jobs, target)
        .await
        .expect("a batch Job with the target member set should appear")
}

/// A `Snapshot`'s `metadata.uid` (once created). Panics if absent — every persisted
/// object has one.
async fn snapshot_uid(backups: &Api<Snapshot>, name: &str) -> String {
    backups
        .get(name)
        .await
        .unwrap_or_else(|e| panic!("get Snapshot {name}: {e}"))
        .uid()
        .unwrap_or_else(|| panic!("Snapshot {name} must have a uid"))
}

/// Create `count` manual `Snapshot`s (origin manual, `deletionPolicy: Delete`) from
/// `names`, wait each Succeeded, and return their UIDs as a set. Deletion of any of
/// these flows through the batch dispatcher (policy `Delete` + a recorded kopia id).
async fn seed_manual_snapshots(
    backups: &Api<Snapshot>,
    policy: &str,
    names: &[&str],
) -> BTreeSet<String> {
    for n in names {
        backups
            .create(
                &PostParams::default(),
                &cr(snapshot_json(
                    E2E_NAMESPACE,
                    n,
                    policy,
                    serde_json::json!({ "deletionPolicy": "Delete" }),
                )),
            )
            .await
            .unwrap_or_else(|e| panic!("create manual Snapshot {n}: {e}"));
    }
    let mut uids = BTreeSet::new();
    for n in names {
        wait_phase(backups, n, "Succeeded")
            .await
            .unwrap_or_else(|e| panic!("manual Snapshot {n} should reach Succeeded: {e}"));
        uids.insert(snapshot_uid(backups, n).await);
    }
    uids
}

/// Delete every named `Snapshot`, then wait until all of them show a
/// `deletionTimestamp` via the API — a settle so a loaded box can't let a delete
/// slip past a threshold/window check before it registers. Result-returning core so
/// the throttle scenario (whose env mutation must be restored even on failure) can
/// `?`-propagate instead of panicking past its restore.
async fn delete_snapshots_and_settle(
    backups: &Api<Snapshot>,
    names: &[&str],
) -> anyhow::Result<()> {
    for n in names {
        backups
            .delete(n, &DeleteParams::default())
            .await
            .map_err(|e| anyhow::anyhow!("delete Snapshot {n}: {e}"))?;
    }
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    wait_until(
        "all deleted Snapshots show a deletionTimestamp",
        Duration::from_secs(60),
        poll_interval(),
        || {
            let backups = backups.clone();
            let names = names.clone();
            async move {
                for n in &names {
                    match backups.get_opt(n).await? {
                        // Already gone (drained faster than we polled) counts as settled.
                        None => continue,
                        Some(s) if s.meta().deletion_timestamp.is_some() => continue,
                        Some(_) => return Ok(None),
                    }
                }
                Ok(Some(()))
            }
        },
    )
    .await
    .map(|_| ())
}

/// Panicking wrapper over [`delete_snapshots_and_settle`] for the scenarios without a
/// must-restore mutation window.
async fn delete_and_settle(backups: &Api<Snapshot>, names: &[&str]) {
    delete_snapshots_and_settle(backups, names)
        .await
        .expect("delete + settle deletionTimestamps");
}

/// Wait until all named `Snapshot`s are gone from the API (their finalizers
/// released) within `timeout`.
async fn wait_all_drained(backups: &Api<Snapshot>, names: &[&str], timeout: Duration) {
    for n in names {
        wait_until(
            &format!("{n} drains (finalizer released)"),
            timeout,
            poll_interval(),
            || async { Ok(backups.get_opt(n).await?.is_none().then_some(())) },
        )
        .await
        .unwrap_or_else(|e| panic!("{n} must drain (batch delete releases its finalizer): {e}"));
    }
}

/// Patch the operator controller Deployment's `KOPIUR_MAX_CONCURRENT_DELETE_JOBS`
/// env to `value` (strategic-merge on the container by name, updating the env entry
/// that the chart always renders) and wait for the rollout to complete, so the ONLY
/// running controller pod is the one carrying the new value. `"0"` restores the
/// chart default (uncapped). Mirrors `leader_election::scale_controller`'s
/// Deployment-mutation style.
async fn set_delete_job_cap(client: &Client, value: &str) -> anyhow::Result<()> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    api.patch(
        CONTROLLER_DEPLOYMENT,
        &PatchParams::default(),
        &Patch::Strategic(serde_json::json!({
            "spec": { "template": { "spec": { "containers": [
                { "name": CONTROLLER_CONTAINER,
                  "env": [ { "name": MAX_CONCURRENT_DELETE_JOBS_ENV, "value": value } ] }
            ]}}}
        })),
    )
    .await?;
    wait::deployment_ready(client, E2E_NAMESPACE, CONTROLLER_DEPLOYMENT).await
}

/// Recursively chmod this scenario's isolated repo dir via a one-shot root busybox
/// Pod (busybox's default uid 0 can chmod files the mover wrote as 65532). `0555`
/// breaks all writes (the batch delete mover then fails EACCES, modelling a backend
/// outage); `0777` restores writability. The Pod is deleted on completion.
async fn chmod_repo(client: &Client, subpath: &str, mode: &str, tag: &str) -> anyhow::Result<()> {
    let pvc = e2e_consts::isolated_repo_pvc(subpath);
    let pod_name = format!("massdel-outage-chmod-{tag}");
    let pod: Pod = builders::one_shot_pod(
        E2E_NAMESPACE,
        &pod_name,
        &["chmod", "-R", mode, e2e_consts::ISOLATED_REPO_PATH],
        &[(pvc.as_str(), e2e_consts::ISOLATED_REPO_PATH)],
    );
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    // Clear any prior same-named chmod pod (restartPolicy Never pods don't self-clean).
    let _ = pods.delete(&pod_name, &DeleteParams::default()).await;
    wait_until(
        &format!("prior chmod pod {pod_name} gone"),
        Duration::from_secs(60),
        poll_interval(),
        || async { Ok(pods.get_opt(&pod_name).await?.is_none().then_some(())) },
    )
    .await?;
    pods.create(&PostParams::default(), &pod).await?;
    wait::pod_succeeded(client, E2E_NAMESPACE, &pod_name).await?;
    let _ = pods.delete(&pod_name, &DeleteParams::default()).await;
    Ok(())
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

    // Bulk-delete all three (no schedule involved — the owner-independent breaker
    // path), then SETTLE: wait until all three carry a deletionTimestamp before
    // asserting the hold. Without this, a loaded box can evaluate the breaker before
    // every delete has registered in the store; if too few are pending it reads as
    // sub-threshold and a deletion slips through un-held. The settle pins the full
    // wave first so the assertions below see the real at/over-threshold state.
    delete_and_settle(&backups, &NAMES).await;

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

// --- Scenario 9: a HELD wave is not swept by a concurrent PRUNE's batch --------

const HELDPRUNE_SUBPATH: &str = "massdel-heldprune";

/// THE FINAL-REVIEW FLAGSHIP COUNTEREXAMPLE (CRITICAL-1). A breaker-exempt operator
/// PRUNE (GFS retention) firing a batch delete Job on a repository must NOT enroll that
/// repository's breaker-HELD external deletions — before the fire-set fix, the prune's
/// oldest-first batch would sweep up the (older) held externals and delete their kopia
/// data with no acknowledgement.
///
/// Construction on ONE repository (threshold 2):
/// - an AGGRESSIVE scheduled policy (`keepLatest: 1`, `defaultDeletionPolicy: Delete`)
///   whose GFS retention continuously prunes scheduled snapshots (breaker-exempt), and
/// - 3 MANUAL externals (a separate `keepLatest: 20` policy so they are never
///   GFS-pruned; label-scoped retention keeps the two policies' prune sets disjoint)
///   bulk-deleted into a HELD wave.
///
/// While GFS prunes ≥2 scheduled snapshots (real `SnapshotDeleteBatch` Jobs firing on
/// the repo), the 3 held manuals MUST stay terminating with `DeletionHeld=True` and
/// never drain; only the `allow-mass-deletion` ack drains them, and only then does the
/// kopia count drop by exactly 3. A manual draining before the ack is the regression.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn held_wave_not_swept_by_concurrent_prune() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client: Client = world.client().clone();
    ensure_repo(&client, HELDPRUNE_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-massdel-heldprune-repo";
    const POLICY_MANUAL: &str = "e2e-massdel-heldprune-manual-pol";
    const POLICY_SCHED: &str = "e2e-massdel-heldprune-sched-pol";
    const SCHED: &str = "e2e-massdel-heldprune-sched";
    // 3 manuals with threshold 2: even if the delete/settle race lets one slip through
    // before it registers, ≥2 remain pending ⇒ the wave is reliably HELD (scenario 2's
    // proven shape).
    const MANUALS: [&str; 3] = [
        "e2e-massdel-heldprune-m1",
        "e2e-massdel-heldprune-m2",
        "e2e-massdel-heldprune-m3",
    ];

    // threshold 2 → the 3 manual externals trip the breaker. Maintenance + catalog off:
    // this scenario asserts the per-Snapshot DeletionHeld (written on the Snapshot's own
    // reconcile), so no catalog refresh is needed, and less churn keeps the counts clean.
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                HELDPRUNE_SUBPATH,
                serde_json::json!({
                    "deletionProtection": { "threshold": 2 },
                    "maintenance": { "enabled": false }
                }),
            )),
        )
        .await
        .expect("create Repository with a threshold-2 breaker");
    wait_phase(&repos, REPO, "Ready")
        .await
        .expect("Repository should reach Ready");

    // Manual policy: keepLatest 20 so the manuals are NEVER GFS-pruned (they must be the
    // HELD external wave, not prune members).
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                POLICY_MANUAL,
                "Repository",
                REPO,
                serde_json::json!({ "retention": { "keepLatest": 20 } }),
            )),
        )
        .await
        .expect("create the manual SnapshotPolicy");
    // Scheduled policy: keepLatest 1 + Delete so GFS prunes its snapshots aggressively —
    // the continuous, breaker-EXEMPT prune pressure on the SAME repo.
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                POLICY_SCHED,
                "Repository",
                REPO,
                serde_json::json!({ "defaultDeletionPolicy": "Delete", "retention": { "keepLatest": 1 } }),
            )),
        )
        .await
        .expect("create the scheduled SnapshotPolicy keeping 1");

    schedules
        .create(
            &PostParams::default(),
            &cr::<SnapshotSchedule>(schedule_json(SCHED, POLICY_SCHED, "* * * * *")),
        )
        .await
        .expect("create SnapshotSchedule");

    // keepLatest 1 + a live "* * * * *" schedule = CONTINUOUS breaker-EXEMPT prune
    // pressure on the SAME repo: each slot produces a scheduled snapshot and GFS prunes
    // the prior one via its OWN batch delete Job. The held wave must ride out that churn.
    // keepLatest 1 never keeps >=3 scheduled snapshots at once, so the counterexample
    // window below collects DISTINCT prune drains OVER TIME (exactly as scenario 3
    // collects >=3 distinct produced names), NOT a simultaneous backlog.
    let sched_selector = format!("{SCHEDULE_LABEL}={SCHED}");

    // 3 manual Succeeded externals (deletionPolicy Delete), then bulk-delete into a HELD
    // wave (threshold 2).
    seed_manual_snapshots(&backups, POLICY_MANUAL, &MANUALS).await;
    delete_and_settle(&backups, &MANUALS).await;
    for n in MANUALS {
        let cond = wait_condition(&backups, n, "DeletionHeld", "True")
            .await
            .unwrap_or_else(|e| panic!("{n} must be HELD by the mass-deletion breaker: {e}"));
        assert_eq!(
            cond.get("reason").and_then(|r| r.as_str()),
            Some("MassDeletionBreaker"),
            "{n} DeletionHeld reason must be MassDeletionBreaker: {cond}"
        );
    }
    // The ack value the operator surfaces (newest pending external deletionTimestamp).
    let held_msg = status_json(&backups, MANUALS[0])
        .await
        .get("conditions")
        .and_then(|c| c.as_array())
        .and_then(|a| {
            a.iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("DeletionHeld"))
        })
        .and_then(|c| c.get("message").and_then(|m| m.as_str()))
        .map(str::to_string)
        .unwrap_or_default();
    let ack_value = parse_ack_value(&held_msg)
        .unwrap_or_else(|| panic!("the held message must carry an ack value: {held_msg}"));

    // THE COUNTEREXAMPLE WINDOW — the schedule stays LIVE, so prune batches keep firing
    // on this repo. Poll until >=2 DISTINCT scheduled snapshots have drained (seen then
    // gone — pruned + finalizer released), asserting on EVERY poll that all 3 held
    // manuals remain terminating with DeletionHeld=True. A manual vanishing here is the
    // regression: a prune batch swept a HELD external into itself and deleted its kopia
    // data without an ack.
    let mut seen_scheduled: BTreeSet<String> = BTreeSet::new();
    let mut drained_scheduled: BTreeSet<String> = BTreeSet::new();
    let window_deadline = Instant::now() + Duration::from_secs(300);
    loop {
        // (i) all held manuals must still be present, terminating, and DeletionHeld=True.
        for n in MANUALS {
            let s = backups.get_opt(n).await.expect("get held manual").unwrap_or_else(|| {
                panic!(
                    "HELD manual {n} DRAINED while never acked — a concurrent prune's batch swept \
                     a breaker-held external into itself (the CRITICAL-1 regression)"
                )
            });
            assert!(
                s.meta().deletion_timestamp.is_some(),
                "held manual {n} must still be terminating"
            );
            assert!(
                snapshot_is_held(&s),
                "held manual {n} must still carry DeletionHeld=True while the wave is unacked"
            );
        }
        // (ii) track scheduled churn: names seen then gone = pruned+drained.
        let current: BTreeSet<String> = backups
            .list(&ListParams::default().labels(&sched_selector))
            .await
            .expect("list scheduled snapshots")
            .items
            .iter()
            .map(|b| b.name_any())
            .collect();
        seen_scheduled.extend(current.iter().cloned());
        for gone in seen_scheduled.difference(&current) {
            drained_scheduled.insert(gone.clone());
        }
        if drained_scheduled.len() >= 2 {
            break;
        }
        assert!(
            Instant::now() < window_deadline,
            "expected >=2 scheduled prune drains (concurrent prune activity) while the wave was \
             held; saw {drained_scheduled:?}"
        );
        tokio::time::sleep(poll_interval()).await;
    }
    eprintln!(
        "[scenario9] concurrent prune proven: {} scheduled snapshots pruned+drained while the \
         3-manual wave stayed HELD",
        drained_scheduled.len()
    );

    // Freeze scheduled production so the kopia count is stable across the ack, then let
    // GFS converge to keepLatest 1 — STILL asserting the manuals stay held throughout.
    schedules
        .patch(
            SCHED,
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({ "spec": { "schedule": { "suspend": true } } })),
        )
        .await
        .expect("suspend the schedule to freeze the scheduled set");
    let converge_deadline = Instant::now() + Duration::from_secs(180);
    loop {
        for n in MANUALS {
            let s = backups
                .get_opt(n)
                .await
                .expect("get held manual")
                .unwrap_or_else(|| {
                    panic!(
                        "HELD manual {n} DRAINED during convergence while never acked (CRITICAL-1)"
                    )
                });
            assert!(
                snapshot_is_held(&s),
                "held manual {n} must still carry DeletionHeld=True during convergence"
            );
        }
        let list = backups
            .list(&ListParams::default().labels(&sched_selector))
            .await
            .expect("list scheduled snapshots")
            .items;
        let live = list
            .iter()
            .filter(|b| b.meta().deletion_timestamp.is_none())
            .count();
        if live <= 1 && list.len() <= 1 {
            break;
        }
        assert!(
            Instant::now() < converge_deadline,
            "scheduled set should converge to keepLatest 1; still {} present",
            list.len()
        );
        tokio::time::sleep(poll_interval()).await;
    }

    // Kopia count while HELD (schedule suspended + converged): must still include all 3
    // held manuals' snapshots.
    let kopia_held =
        observed_snapshot_count(&client, "e2e-massdel-heldprune-verify-1", HELDPRUNE_SUBPATH).await;

    // Acknowledge the wave → the 3 held manuals drain, and NOW their kopia data goes.
    repos
        .patch(
            REPO,
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({
                "metadata": { "annotations": { ALLOW_MASS_DELETION_ANNOTATION: ack_value } }
            })),
        )
        .await
        .expect("acknowledge the mass-deletion wave");
    wait_all_drained(&backups, &MANUALS, Duration::from_secs(180)).await;
    let kopia_after =
        observed_snapshot_count(&client, "e2e-massdel-heldprune-verify-2", HELDPRUNE_SUBPATH).await;
    assert_eq!(
        kopia_after,
        kopia_held - 3,
        "the ack must delete EXACTLY the 3 held manuals' kopia snapshots (present throughout the \
         prune window); held={kopia_held} after={kopia_after}"
    );

    // Cleanup: remaining scheduled CRs BEFORE the policies/repo (finalizers need the repo).
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
    let _ = schedules.delete(SCHED, &DeleteParams::default()).await;
    let _ = policies
        .delete(POLICY_MANUAL, &DeleteParams::default())
        .await;
    let _ = policies
        .delete(POLICY_SCHED, &DeleteParams::default())
        .await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}

// --- Scenario 5: a single delete still flows through the BATCH dispatcher ------

const SINGLE_SUBPATH: &str = "massdel-single";

/// The batch dispatcher unifies EVERY delete onto the per-repository batch path — a
/// batch of ONE is still one `SnapshotDeleteBatch` Job, not a per-CR `{name}-delete`
/// Job. Deleting one manual `Succeeded` snapshot (deletionPolicy `Delete`, default
/// threshold, so never held) must: (a) create exactly ONE batch Job whose
/// `delete-members` is exactly this CR's UID; (b) drain the CR; (c) drop the kopia
/// count by 1. Pins the batch-of-1 unification the M5a swap introduced.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn single_snapshot_delete_still_releases() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client: Client = world.client().clone();
    ensure_repo(&client, SINGLE_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-massdel-single-repo";
    const POLICY: &str = "e2e-massdel-single-pol";
    const SNAP: &str = "e2e-massdel-single-1";

    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                SINGLE_SUBPATH,
                serde_json::json!({ "maintenance": { "enabled": false } }),
            )),
        )
        .await
        .expect("create Repository");
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
                // keepLatest generous so no GFS prune races the manual deletes.
                serde_json::json!({ "retention": { "keepLatest": 20 } }),
            )),
        )
        .await
        .expect("create SnapshotPolicy");

    let uids = seed_manual_snapshots(&backups, POLICY, &[SNAP]).await;
    let kopia_before =
        observed_snapshot_count(&client, "e2e-massdel-single-verify-1", SINGLE_SUBPATH).await;
    assert_eq!(
        kopia_before, 1,
        "the single manual snapshot must exist in kopia before the delete, got {kopia_before}"
    );

    delete_and_settle(&backups, &[SNAP]).await;

    // (a) exactly one batch Job for this repo, delete-members == exactly this UID.
    let observed = wait_until(
        "a batch delete Job appears for the single delete",
        default_timeout(),
        poll_interval(),
        || {
            let jobs = jobs.clone();
            let mine = uids.clone();
            async move {
                let js = my_batch_jobs(&jobs, &mine).await;
                Ok((!js.is_empty()).then_some(js))
            }
        },
    )
    .await
    .expect("the single delete must produce a batch Job (batch-of-1 unification)");
    assert_eq!(
        observed.len(),
        1,
        "a single delete must produce EXACTLY ONE batch Job, got {}: {:?}",
        observed.len(),
        observed.iter().map(|j| j.name_any()).collect::<Vec<_>>()
    );
    assert_eq!(
        batch_members(&observed[0]),
        uids,
        "the batch Job's delete-members must be exactly the deleted CR's UID"
    );

    // (b) the CR drains.
    wait_all_drained(&backups, &[SNAP], Duration::from_secs(180)).await;

    // (c) kopia -1.
    let kopia_after =
        observed_snapshot_count(&client, "e2e-massdel-single-verify-2", SINGLE_SUBPATH).await;
    assert_eq!(
        kopia_after,
        kopia_before - 1,
        "the batch-of-1 delete must remove the kopia snapshot; before={kopia_before} after={kopia_after}"
    );

    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}

// --- Scenario 6: concurrent batches do not overlap (uncapped default) ----------

const NOOVERLAP_SUBPATH: &str = "massdel-nooverlap";

/// Two disjoint delete waves make two disjoint batch Jobs — the no-overlap invariant
/// under the default (uncapped) concurrency. FIVE manual `Succeeded` snapshots (repo
/// threshold left DEFAULT 10 — five deletes stay below it, so nothing is held).
/// Delete 3 → a batch Job with EXACTLY those 3 UIDs. While it is (ideally) live,
/// delete the other 2 → a SECOND batch Job with EXACTLY those 2 UIDs, sharing NO
/// member with the first. All 5 drain; kopia -5.
///
/// Timing note (brief §6): a filesystem batch delete can complete before the second
/// wave is issued, so simultaneous LIVENESS is best-effort (logged). The HARD,
/// timing-robust assertions are the per-wave member sets, their DISJOINTNESS (the
/// no-overlap invariant), the two being SEPARATE Jobs, and total drain.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn concurrent_batches_do_not_overlap() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client: Client = world.client().clone();
    ensure_repo(&client, NOOVERLAP_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-massdel-nooverlap-repo";
    const POLICY: &str = "e2e-massdel-nooverlap-pol";
    const NAMES: [&str; 5] = [
        "e2e-massdel-noov-1",
        "e2e-massdel-noov-2",
        "e2e-massdel-noov-3",
        "e2e-massdel-noov-4",
        "e2e-massdel-noov-5",
    ];

    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                NOOVERLAP_SUBPATH,
                serde_json::json!({ "maintenance": { "enabled": false } }),
            )),
        )
        .await
        .expect("create Repository");
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
                serde_json::json!({ "retention": { "keepLatest": 20 } }),
            )),
        )
        .await
        .expect("create SnapshotPolicy");

    seed_manual_snapshots(&backups, POLICY, &NAMES).await;
    let a_names = &NAMES[..3];
    let b_names = &NAMES[3..];
    let mut set_a: BTreeSet<String> = BTreeSet::new();
    for n in a_names {
        set_a.insert(snapshot_uid(&backups, n).await);
    }
    let mut set_b: BTreeSet<String> = BTreeSet::new();
    for n in b_names {
        set_b.insert(snapshot_uid(&backups, n).await);
    }
    assert!(
        set_a.is_disjoint(&set_b),
        "the two waves must be disjoint snapshot sets by construction"
    );

    let kopia_before =
        observed_snapshot_count(&client, "e2e-massdel-noov-verify-1", NOOVERLAP_SUBPATH).await;
    assert_eq!(
        kopia_before, 5,
        "five snapshots must exist before, got {kopia_before}"
    );

    // Wave 1: delete 3 → the batch Job with exactly those 3.
    delete_and_settle(&backups, a_names).await;
    let batch_a = wait_batch_with_members(&jobs, &set_a).await;
    let a_live_at_capture = batch_job_state(&batch_a) == BatchJobState::Live;

    // Wave 2: delete the other 2 → a SECOND, disjoint batch Job.
    delete_and_settle(&backups, b_names).await;
    let batch_b = wait_batch_with_members(&jobs, &set_b).await;
    // Best-effort overlap observation: was wave-1's Job still live when wave-2's appeared?
    let a_live_when_b_appeared = jobs
        .get_opt(&batch_a.name_any())
        .await
        .ok()
        .flatten()
        .map(|j| batch_job_state(&j) == BatchJobState::Live)
        .unwrap_or(false);

    // HARD (timing-robust) assertions.
    assert_eq!(
        batch_members(&batch_a),
        set_a,
        "wave-1 batch Job must cover EXACTLY the 3 co-deleted UIDs"
    );
    assert_eq!(
        batch_members(&batch_b),
        set_b,
        "wave-2 batch Job must cover EXACTLY the 2 co-deleted UIDs"
    );
    assert_ne!(
        batch_a.name_any(),
        batch_b.name_any(),
        "the two waves must be SEPARATE batch Jobs (concurrency, not one merged Job)"
    );
    assert!(
        batch_members(&batch_b).is_disjoint(&batch_members(&batch_a)),
        "NO-OVERLAP: wave-2's batch must not re-enroll any wave-1 member"
    );
    eprintln!(
        "[scenario6] no-overlap proven; wave-1 live when captured={a_live_at_capture}, \
         wave-1 live when wave-2 appeared={a_live_when_b_appeared} \
         (true = simultaneous concurrency observed; false = disjointness-only)"
    );

    // All 5 drain; kopia -5.
    wait_all_drained(&backups, &NAMES, Duration::from_secs(300)).await;
    let kopia_after =
        observed_snapshot_count(&client, "e2e-massdel-noov-verify-2", NOOVERLAP_SUBPATH).await;
    assert_eq!(
        kopia_after,
        kopia_before - 5,
        "both waves must delete all 5 kopia snapshots; before={kopia_before} after={kopia_after}"
    );

    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}

// --- Scenario 7: the concurrency throttle caps live batch Jobs -----------------

const THROTTLE_SUBPATH: &str = "massdel-throttle";

/// With `KOPIUR_MAX_CONCURRENT_DELETE_JOBS=1` on the operator Deployment, at most ONE
/// live batch delete Job may exist cluster-wide at any instant — a second wave is
/// throttled until the first goes terminal. Same shape as scenario 6 (3 then 2), but
/// the load-bearing, timing-robust assertion is the INVARIANT (never two live at
/// once), enforced by the cap regardless of how fast a batch runs; both waves still
/// drain and delete their kopia snapshots.
///
/// Deployment mutation is done the `scale_controller` way (patch + rollout wait) and
/// RESTORED (env back to `0` = uncapped + rollout wait) on EVERY exit, so no other
/// binary in the same `--test-threads=1` run inherits the cap. The mutation window is
/// entered only AFTER seeding (which uses panicking helpers); everything inside it
/// `?`-propagates, so a failure never skips the restore. Returns `Result` for that.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn throttle_caps_concurrent_batch_jobs() -> anyhow::Result<()> {
    let Some(world) = World::connect().await else {
        return Ok(());
    };
    world.ensure(&[Need::Filesystem]).await?;
    let client: Client = world.client().clone();
    ensure_repo(&client, THROTTLE_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-massdel-throttle-repo";
    const POLICY: &str = "e2e-massdel-throttle-pol";
    const NAMES: [&str; 5] = [
        "e2e-massdel-thr-1",
        "e2e-massdel-thr-2",
        "e2e-massdel-thr-3",
        "e2e-massdel-thr-4",
        "e2e-massdel-thr-5",
    ];

    // Setup + seed while UNCAPPED (panicking helpers fine — no env mutation yet).
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                THROTTLE_SUBPATH,
                serde_json::json!({ "maintenance": { "enabled": false } }),
            )),
        )
        .await
        .expect("create Repository");
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
                serde_json::json!({ "retention": { "keepLatest": 20 } }),
            )),
        )
        .await
        .expect("create SnapshotPolicy");
    seed_manual_snapshots(&backups, POLICY, &NAMES).await;
    let a_names = &NAMES[..3];
    let b_names = &NAMES[3..];
    let mut set_a: BTreeSet<String> = BTreeSet::new();
    for n in a_names {
        set_a.insert(snapshot_uid(&backups, n).await);
    }
    let mut set_b: BTreeSet<String> = BTreeSet::new();
    for n in b_names {
        set_b.insert(snapshot_uid(&backups, n).await);
    }
    let mine: BTreeSet<String> = set_a.union(&set_b).cloned().collect();
    let kopia_before =
        observed_snapshot_count(&client, "e2e-massdel-throttle-verify-1", THROTTLE_SUBPATH).await;
    assert_eq!(
        kopia_before, 5,
        "five snapshots must exist before, got {kopia_before}"
    );

    // Enter the mutation window: cap concurrent batch delete Jobs to 1. Guard the
    // SET too: if the env patch applied but the rollout wait then errored, the env is
    // already "1", so the restore below MUST still run — don't `?`-return here.
    let set_result = set_delete_job_cap(&client, "1").await;

    // Everything inside `?`-propagates so a failure still hits the restore below.
    let body: anyhow::Result<()> = async {
        // If the cap could not be set, exercise nothing under it (restore still runs).
        if set_result.is_err() {
            return Ok(());
        }
        // Wave 1: delete 3, wait for its batch Job to fire.
        delete_snapshots_and_settle(&backups, a_names).await?;
        find_batch_with_members(&jobs, &set_a).await?;
        // Wave 2: delete the other 2 (would fire concurrently if uncapped).
        delete_snapshots_and_settle(&backups, b_names).await?;

        // Drive the drain, asserting the INVARIANT on every poll: at most one LIVE
        // batch Job for this repo at any instant. Poll fast (1s) so a real >1 window
        // (which would persist for a whole batch's lifetime) cannot slip through.
        let mut saw_a = false;
        let mut saw_b = false;
        let deadline = Instant::now() + Duration::from_secs(360);
        loop {
            let js = my_batch_jobs(&jobs, &mine).await;
            let live = js
                .iter()
                .filter(|j| batch_job_state(j) == BatchJobState::Live)
                .count();
            anyhow::ensure!(
                live <= 1,
                "throttle cap=1 violated: {live} live batch Jobs for one repo simultaneously"
            );
            for j in &js {
                let m = batch_members(j);
                if m == set_a {
                    saw_a = true;
                }
                if m == set_b {
                    saw_b = true;
                }
            }
            let mut remaining = 0usize;
            for n in NAMES {
                if backups.get_opt(n).await?.is_some() {
                    remaining += 1;
                }
            }
            if remaining == 0 {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "throttled waves did not all drain within the deadline"
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        anyhow::ensure!(saw_a, "wave-1 batch Job (setA) was never observed");
        anyhow::ensure!(saw_b, "wave-2 batch Job (setB) was never observed");
        Ok(())
    }
    .await;

    // ALWAYS restore the cap to uncapped (chart default `0`) + rollout wait.
    let restore = set_delete_job_cap(&client, "0").await;
    // Best-effort cleanup regardless of outcome.
    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
    // Propagate the cap-set failure first (the body was skipped in that case), then
    // the body's failure, then any restore failure.
    set_result?;
    body?;
    restore?;

    // kopia -5 (verified after the window, uncapped again).
    let kopia_after =
        observed_snapshot_count(&client, "e2e-massdel-throttle-verify-2", THROTTLE_SUBPATH).await;
    anyhow::ensure!(
        kopia_after == kopia_before - 5,
        "throttled waves must still delete all 5 kopia snapshots; before={kopia_before} after={kopia_after}"
    );
    Ok(())
}

// --- Scenario 8: batch delete retries after a repository outage -----------------

const OUTAGE_SUBPATH: &str = "massdel-outage";

/// Fail-safe under a backend outage + convergence after recovery. THREE manual
/// `Succeeded` snapshots on an isolated filesystem repo; then the repo dir is flipped
/// READ-ONLY (`chmod 0555` via a root helper Pod — the runtime analogue of the
/// `/ro-repo` seed) so the batch delete mover fails EACCES on write. Deleting the CRs
/// must: keep them terminating (finalizers HELD, no kopia data touched) while the
/// batch Job fails, is reaped, and refires — observed as ≥2 distinct batch Job
/// GENERATIONS (same member set, different Job UID; the deterministic name is reused).
/// Restoring writability (`chmod 0777`) then drains all three and REALLY deletes the
/// kopia snapshots (count -3).
///
/// The RO flip is confined to THIS scenario's isolated subpath, so a mid-scenario
/// failure that leaves it read-only cannot affect any other scenario or test binary
/// (fresh clusters reseed 0777).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn batch_delete_retries_after_repo_outage() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client: Client = world.client().clone();
    ensure_repo(&client, OUTAGE_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-massdel-outage-repo";
    const POLICY: &str = "e2e-massdel-outage-pol";
    const NAMES: [&str; 3] = [
        "e2e-massdel-outage-1",
        "e2e-massdel-outage-2",
        "e2e-massdel-outage-3",
    ];

    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                OUTAGE_SUBPATH,
                serde_json::json!({ "maintenance": { "enabled": false } }),
            )),
        )
        .await
        .expect("create Repository");
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
                serde_json::json!({ "retention": { "keepLatest": 20 } }),
            )),
        )
        .await
        .expect("create SnapshotPolicy");

    let uids = seed_manual_snapshots(&backups, POLICY, &NAMES).await;
    let kopia_before =
        observed_snapshot_count(&client, "e2e-massdel-outage-verify-1", OUTAGE_SUBPATH).await;
    assert_eq!(
        kopia_before, 3,
        "three snapshots must exist before the outage, got {kopia_before}"
    );

    // BREAK the backend: flip the repo dir read-only. The batch delete mover fails
    // EACCES on write against it (like the `/ro-repo` terminal-failure seed).
    chmod_repo(&client, OUTAGE_SUBPATH, "0555", "break")
        .await
        .expect("flip the isolated repo dir read-only");

    delete_and_settle(&backups, &NAMES).await;

    // Observe ≥2 distinct batch Job GENERATIONS (fail → reap → refire) while NO CR
    // drains — every finalizer is held because no kopia delete can succeed.
    let mut generations: BTreeSet<String> = BTreeSet::new();
    let deadline = Instant::now() + Duration::from_secs(480);
    loop {
        for j in my_batch_jobs(&jobs, &uids).await {
            if let Some(u) = j.uid() {
                generations.insert(u);
            }
        }
        for n in NAMES {
            let s = backups
                .get_opt(n)
                .await
                .expect("get Snapshot")
                .unwrap_or_else(|| {
                    panic!("{n} must NOT drain while the backend is broken (fail-safe)")
                });
            assert!(
                s.meta().deletion_timestamp.is_some(),
                "{n} must still be terminating while the backend is broken"
            );
            assert!(
                s.finalizers()
                    .iter()
                    .any(|f| f == SNAPSHOT_CLEANUP_FINALIZER),
                "{n} must still hold its cleanup finalizer while the backend is broken"
            );
        }
        if generations.len() >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expected ≥2 batch Job generations (fail→reap→refire) under outage; saw {}",
            generations.len()
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // RESTORE the backend: writability back. The next refired batch Job connects and
    // deletes for real.
    chmod_repo(&client, OUTAGE_SUBPATH, "0777", "heal")
        .await
        .expect("restore the isolated repo dir writability");

    // Full drain + real kopia deletion after recovery.
    wait_all_drained(&backups, &NAMES, Duration::from_secs(360)).await;
    let kopia_after =
        observed_snapshot_count(&client, "e2e-massdel-outage-verify-2", OUTAGE_SUBPATH).await;
    assert_eq!(
        kopia_after,
        kopia_before - 3,
        "the recovered batch must delete all 3 kopia snapshots; before={kopia_before} after={kopia_after}"
    );

    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}

// --- Policy-cascade scenarios (feat/policy-cascade-adoption, M8) ----------------
// The SnapshotPolicy-deletion cascade is the sibling of the schedule-deletion cascade
// (scenario 1): deleting a `SnapshotPolicy` cascades onto the `Snapshot` CRs carrying
// its config label, governed by `spec.deletion.onPolicyDelete` (safe-default `Retain`,
// opt-in `Delete`). These prove the Retain cascade keeps kopia + re-catalogs (10), the
// `Delete` opt-in is breaker-gated WITHOUT wedging the policy finalizer (11), and a
// simultaneous schedule+policy delete drains cleanly to Retain (12).

/// This policy's config-labeled `Snapshot` children.
async fn config_children(backups: &Api<Snapshot>, policy: &str) -> Vec<Snapshot> {
    backups
        .list(&ListParams::default().labels(&format!("{CONFIG_LABEL}={policy}")))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

/// This repository's discovered rows (`origin=discovered` + the repository UID).
async fn repo_discovered_rows(backups: &Api<Snapshot>, repo_uid: &str) -> Vec<Snapshot> {
    backups
        .list(&ListParams::default().labels(&format!(
            "{ORIGIN_LABEL}=discovered,{REPOSITORY_UID_LABEL}={repo_uid}"
        )))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

// --- Scenario 10: policy-cascade (Retain) cleans CRs, keeps kopia, re-catalogs --

const POLCASC_RETAIN_SUBPATH: &str = "polcasc-retain";

/// Deleting a `SnapshotPolicy` whose `onPolicyDelete` is the safe-default `Retain`
/// removes EVERY `Snapshot` CR carrying its config label — a schedule-produced one AND
/// a MANUAL one created by plain apply with only a `spec.policyRef` (proving the M0
/// webhook config-label stamp end-to-end) — WITHOUT hanging the policy finalizer or
/// touching kopia. Each cascaded CR fires a `SnapshotRetainedOnPolicyDelete` Warning;
/// the retained kopia snapshots re-catalog as `discovered`; and a pre-existing
/// (different-identity) discovered row is untouched throughout.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn policy_cascade_delete_cleans_crs_keeps_kopia() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client: Client = world.client().clone();
    ensure_repo(&client, POLCASC_RETAIN_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-polcasc-retain-repo";
    const POLICY: &str = "e2e-polcasc-retain-pol";
    const SCHED: &str = "e2e-polcasc-retain-sched";
    const SEED_POL: &str = "e2e-polcasc-retain-seed-pol";
    const SEED_SNAP: &str = "e2e-polcasc-retain-seed-1";
    const MANUAL: &str = "e2e-polcasc-retain-manual";

    // Fast catalog refresh so rediscovery is prompt; maintenance off. NO
    // deletionProtection — the Retain cascade never counts against the breaker.
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                POLCASC_RETAIN_SUBPATH,
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
    let repo_uid = repos
        .get(REPO)
        .await
        .expect("get Repository")
        .uid()
        .expect("Repository uid");

    // Main policy: onPolicyDelete unset ⇒ Retain default; generous retention.
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

    // Pre-existing DISCOVERED row (different identity): a seed policy produces one
    // snapshot; deleting its CR (Retain) re-catalogs it as a discovered row, then the
    // seed policy is removed so NO live policy matches its identity (it must never be
    // adopted or cascaded — it is the untouched control).
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                SEED_POL,
                "Repository",
                REPO,
                serde_json::json!({ "retention": { "keepLatest": 20 } }),
            )),
        )
        .await
        .expect("create seed SnapshotPolicy");
    backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                SEED_SNAP,
                SEED_POL,
                serde_json::json!({ "deletionPolicy": "Retain" }),
            )),
        )
        .await
        .expect("create seed Snapshot");
    wait_phase(&backups, SEED_SNAP, "Succeeded")
        .await
        .expect("seed Snapshot should Succeed");
    // Retire the SEED POLICY (Retain cascade keeps the child's kopia snapshot) BEFORE
    // the snapshot re-catalogs. This is load-bearing: a LIVE identity-matching policy
    // would AUTO-ADOPT the re-cataloged snapshot (flipping it out of `discovered`), so
    // the seed policy must be gone first for the seed to persist as the untouched
    // `discovered` control. A terminating policy never runs the adoption path.
    policies
        .delete(SEED_POL, &DeleteParams::default())
        .await
        .expect("delete the seed SnapshotPolicy (Retain cascade ⇒ kopia kept ⇒ rediscovered)");
    let seed_row = wait_until(
        "the seed snapshot re-catalogs as a discovered row once the seed policy is retired",
        default_timeout(),
        poll_interval(),
        || {
            let backups = backups.clone();
            let policies = policies.clone();
            let repo_uid = repo_uid.clone();
            async move {
                // Only accept the discovered row once SEED_POL is fully gone, so no
                // live matcher can adopt it out from under this control.
                if policies.get_opt(SEED_POL).await?.is_some() {
                    return Ok(None);
                }
                let rows = repo_discovered_rows(&backups, &repo_uid).await;
                Ok(rows.into_iter().next())
            }
        },
    )
    .await
    .expect("the seed snapshot must re-catalog as a discovered row (no live policy to adopt it)");
    let seed_disc_name = seed_row.name_any();
    let seed_uid = seed_row.uid().expect("seed discovered row uid");

    // Schedule-produced child.
    schedules
        .create(
            &PostParams::default(),
            &cr::<SnapshotSchedule>(schedule_json(SCHED, POLICY, "* * * * *")),
        )
        .await
        .expect("create SnapshotSchedule");
    let sched_selector = format!("{SCHEDULE_LABEL}={SCHED}");
    wait_until(
        "the schedule produces ≥1 Succeeded snapshot",
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
                let ok = list.iter().any(|b| {
                    serde_json::to_value(b)
                        .unwrap_or_default()
                        .pointer("/status/phase")
                        .and_then(|p| p.as_str())
                        == Some("Succeeded")
                });
                Ok(ok.then_some(()))
            }
        },
    )
    .await
    .expect("the schedule should produce a Succeeded snapshot");
    // Freeze production so counts are stable across the cascade.
    schedules
        .patch(
            SCHED,
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({ "spec": { "schedule": { "suspend": true } } })),
        )
        .await
        .expect("suspend schedule");

    // MANUAL snapshot by plain apply with ONLY a policyRef (no config label): the
    // admission webhook must STAMP the config label so it is GFS/cascade-visible.
    let created: Snapshot = backups
        .create(
            &PostParams::default(),
            &cr::<Snapshot>(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": { "name": MANUAL, "namespace": E2E_NAMESPACE },
                "spec": { "policyRef": { "name": POLICY }, "deletionPolicy": "Delete" }
            })),
        )
        .await
        .expect("create the manual Snapshot with only a policyRef");
    assert_eq!(
        created.labels().get(CONFIG_LABEL).map(String::as_str),
        Some(POLICY),
        "the admission webhook must STAMP the config label on a policyRef-only manual Snapshot \
         (M0), or it would be invisible to GFS retention and the policy cascade"
    );
    wait_phase(&backups, MANUAL, "Succeeded")
        .await
        .expect("the manual Snapshot should Succeed");

    // The config-labeled child set the cascade must clean (scheduled + manual).
    let child_names: Vec<String> = config_children(&backups, POLICY)
        .await
        .iter()
        .map(|b| b.name_any())
        .collect();
    assert!(
        child_names.len() >= 2 && child_names.iter().any(|n| n == MANUAL),
        "expected ≥2 config-labeled children (scheduled + the stamped manual); got {child_names:?}"
    );

    let kopia_before = observed_snapshot_count(
        &client,
        "e2e-polcasc-retain-verify-1",
        POLCASC_RETAIN_SUBPATH,
    )
    .await;
    assert!(
        kopia_before >= 3,
        "kopia should hold the scheduled + manual + seed snapshots (≥3) before the cascade; got {kopia_before}"
    );

    // Delete ONLY the main policy.
    policies
        .delete(POLICY, &DeleteParams::default())
        .await
        .expect("delete the main SnapshotPolicy (the cascade trigger)");

    // Drain: every config-labeled child gone AND the policy finalizer released, while
    // the pre-existing discovered row endures with its ORIGINAL uid throughout (a
    // cascade wrongly touching it would delete/replace it).
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let seed_now = backups
            .get_opt(&seed_disc_name)
            .await
            .expect("get seed discovered row");
        assert!(
            seed_now
                .as_ref()
                .and_then(|s| s.uid())
                .is_some_and(|u| u == seed_uid),
            "the pre-existing discovered row must be UNTOUCHED by the policy cascade (same uid) \
             throughout the drain"
        );
        let children = config_children(&backups, POLICY).await;
        let policy_gone = policies
            .get_opt(POLICY)
            .await
            .expect("get policy")
            .is_none();
        if children.is_empty() && policy_gone {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the policy cascade must drain every config-labeled child and RELEASE the policy \
             finalizer; still {} child(ren), policy_gone={policy_gone}",
            children.len()
        );
        tokio::time::sleep(poll_interval()).await;
    }

    // Each vanished child fired a `SnapshotRetainedOnPolicyDelete` Warning.
    for name in &child_names {
        assert!(
            snapshot_warning_event_exists(
                &client,
                E2E_NAMESPACE,
                name,
                "SnapshotRetainedOnPolicyDelete"
            )
            .await,
            "cascade-retained snapshot {name} must get a SnapshotRetainedOnPolicyDelete Warning"
        );
    }

    // kopia UNCHANGED — the Retain cascade kept every snapshot.
    let kopia_after = observed_snapshot_count(
        &client,
        "e2e-polcasc-retain-verify-2",
        POLCASC_RETAIN_SUBPATH,
    )
    .await;
    assert_eq!(
        kopia_after, kopia_before,
        "a Retain-cascade policy delete MUST NOT touch kopia data; before={kopia_before} after={kopia_after}"
    );

    // The retained kopia snapshots re-catalog as discovered rows (the child set + the
    // untouched seed row).
    wait_until(
        "the cascade-retained snapshots re-catalog as discovered rows",
        default_timeout(),
        poll_interval(),
        || {
            let backups = backups.clone();
            let repo_uid = repo_uid.clone();
            let want = child_names.len() as i64 + 1;
            async move {
                let rows = repo_discovered_rows(&backups, &repo_uid).await;
                Ok((rows.len() as i64 >= want).then_some(()))
            }
        },
    )
    .await
    .expect("the retained snapshots must re-materialize as discovered rows");
    // The pre-existing discovered row still endures (final check).
    assert!(
        backups
            .get_opt(&seed_disc_name)
            .await
            .expect("get seed row")
            .and_then(|s| s.uid())
            .is_some_and(|u| u == seed_uid),
        "the pre-existing discovered row must survive the whole cascade untouched"
    );

    // Cleanup: discovered rows are forced-Retain; remove Snapshot CRs before the repo.
    for r in repo_discovered_rows(&backups, &repo_uid).await {
        let _ = backups
            .delete(&r.name_any(), &DeleteParams::default())
            .await;
    }
    let _ = schedules.delete(SCHED, &DeleteParams::default()).await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}

// --- Scenario 11: policy-cascade Delete opt-in is breaker-gated -----------------

const POLCASC_DELETE_SUBPATH: &str = "polcasc-delete";

/// The `onPolicyDelete: Delete` opt-in cascades EXTERNAL deletions, so it is subject
/// to the per-repository mass-deletion breaker. Deleting a policy whose ≥3 children
/// exceed the repo threshold HOLDS them (`DeletionHeld=True`, no kopia data touched),
/// yet the POLICY CR's finalizer still RELEASES (a terminating breaker-held child must
/// resolve to "no work" — the finalizer must never wedge waiting on an ack that would
/// never come once the policy is gone). The `allow-mass-deletion` ack then drains the
/// wave and really deletes the kopia snapshots.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn policy_cascade_opt_in_delete_is_breaker_gated() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client: Client = world.client().clone();
    ensure_repo(&client, POLCASC_DELETE_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-polcasc-delete-repo";
    const POLICY: &str = "e2e-polcasc-delete-pol";
    const NAMES: [&str; 3] = [
        "e2e-polcasc-delete-1",
        "e2e-polcasc-delete-2",
        "e2e-polcasc-delete-3",
    ];

    // threshold: 2 → the 3 cascaded external deletions trip the breaker. Fast refresh
    // so the repo's MassDeletionHeld condition appears promptly; maintenance off.
    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                POLCASC_DELETE_SUBPATH,
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

    // The opt-in: spec.deletion.onPolicyDelete: Delete.
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                POLICY,
                "Repository",
                REPO,
                serde_json::json!({
                    "deletion": { "onPolicyDelete": "Delete" },
                    "retention": { "keepLatest": 20 }
                }),
            )),
        )
        .await
        .expect("create SnapshotPolicy with onPolicyDelete: Delete");

    // Three config-labeled manual children (deletionPolicy Delete), all Succeeded.
    seed_manual_snapshots(&backups, POLICY, &NAMES).await;
    let kopia_before = observed_snapshot_count(
        &client,
        "e2e-polcasc-delete-verify-1",
        POLCASC_DELETE_SUBPATH,
    )
    .await;
    assert_eq!(
        kopia_before, 3,
        "the three children must all exist in kopia before the cascade, got {kopia_before}"
    );

    // Delete the policy → the Delete-mode cascade bare-deletes the 3 children as
    // EXTERNAL deletions → the breaker HOLDS them.
    policies
        .delete(POLICY, &DeleteParams::default())
        .await
        .expect("delete the SnapshotPolicy (Delete-cascade trigger)");

    // (a) all three children go terminating with DeletionHeld=True / MassDeletionBreaker.
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

    // (b) the POLICY finalizer RELEASES even though its children are held — the
    // load-bearing anti-wedge guarantee (a terminating held child ⇒ no cascade work).
    wait_until(
        "the policy finalizer releases despite breaker-held children",
        Duration::from_secs(150),
        poll_interval(),
        || async { Ok(policies.get_opt(POLICY).await?.is_none().then_some(())) },
    )
    .await
    .expect(
        "the SnapshotPolicy CR must be released even while its Delete-cascaded children are HELD",
    );

    // (c) the children are STILL held after the policy is gone, and kopia is untouched.
    for n in NAMES {
        let s = backups
            .get_opt(n)
            .await
            .expect("get held child")
            .unwrap_or_else(|| panic!("held child {n} must still exist"));
        assert!(
            s.meta().deletion_timestamp.is_some() && snapshot_is_held(&s),
            "held child {n} must stay terminating + DeletionHeld after the policy released"
        );
    }
    let kopia_held = observed_snapshot_count(
        &client,
        "e2e-polcasc-delete-verify-2",
        POLCASC_DELETE_SUBPATH,
    )
    .await;
    assert_eq!(
        kopia_held, kopia_before,
        "NO kopia data may be deleted while the cascaded wave is HELD; before={kopia_before} held={kopia_held}"
    );

    // (d) ack via the repository annotation → the wave drains and REALLY deletes kopia.
    let held_msg = status_json(&backups, NAMES[0])
        .await
        .get("conditions")
        .and_then(|c| c.as_array())
        .and_then(|a| {
            a.iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("DeletionHeld"))
        })
        .and_then(|c| c.get("message").and_then(|m| m.as_str()))
        .map(str::to_string)
        .unwrap_or_default();
    let ack_value = parse_ack_value(&held_msg)
        .unwrap_or_else(|| panic!("the held message must carry an ack value: {held_msg}"));
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
    wait_all_drained(&backups, &NAMES, Duration::from_secs(180)).await;
    let kopia_after = observed_snapshot_count(
        &client,
        "e2e-polcasc-delete-verify-3",
        POLCASC_DELETE_SUBPATH,
    )
    .await;
    assert_eq!(
        kopia_after,
        kopia_before - 3,
        "the acked cascade must delete all 3 kopia snapshots; before={kopia_before} after={kopia_after}"
    );

    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}

// --- Scenario 12: simultaneous schedule + policy delete drains to Retain --------

const POLCASC_SIMUL_SUBPATH: &str = "polcasc-simul";

/// Deleting a `SnapshotSchedule` and its `SnapshotPolicy` back-to-back (both at their
/// safe-default `Retain`) must NOT wedge: every produced `Snapshot` CR drains (no
/// stuck finalizer under the overlapping cascades), the kopia data is intact, and NO
/// `SnapshotDeleteBatch` Job ever fires (nothing is deleted kopia-side).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn schedule_and_policy_simultaneous_delete_drains_to_retain() {
    let Some(world) = World::connect().await else {
        return;
    };
    world
        .ensure(&[Need::Filesystem])
        .await
        .expect("provision filesystem fixtures");
    let client: Client = world.client().clone();
    ensure_repo(&client, POLCASC_SIMUL_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    const REPO: &str = "e2e-polcasc-simul-repo";
    const POLICY: &str = "e2e-polcasc-simul-pol";
    const SCHED: &str = "e2e-polcasc-simul-sched";

    repos
        .create(
            &PostParams::default(),
            &cr(repository_json(
                REPO,
                POLCASC_SIMUL_SUBPATH,
                serde_json::json!({ "maintenance": { "enabled": false } }),
            )),
        )
        .await
        .expect("create Repository");
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
    schedules
        .create(
            &PostParams::default(),
            &cr::<SnapshotSchedule>(schedule_json(SCHED, POLICY, "* * * * *")),
        )
        .await
        .expect("create SnapshotSchedule");

    // Wait for ≥2 Succeeded scheduled children, then freeze the produced set.
    let sched_selector = format!("{SCHEDULE_LABEL}={SCHED}");
    wait_until(
        "the schedule produces ≥2 Succeeded snapshots",
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
                let n = list
                    .iter()
                    .filter(|b| {
                        serde_json::to_value(b)
                            .unwrap_or_default()
                            .pointer("/status/phase")
                            .and_then(|p| p.as_str())
                            == Some("Succeeded")
                    })
                    .count();
                Ok((n >= 2).then_some(()))
            }
        },
    )
    .await
    .expect("the schedule should produce ≥2 Succeeded snapshots");
    schedules
        .patch(
            SCHED,
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({ "spec": { "schedule": { "suspend": true } } })),
        )
        .await
        .expect("suspend schedule to freeze the produced set");
    tokio::time::sleep(Duration::from_secs(12)).await;

    let children: Vec<Snapshot> = config_children(&backups, POLICY).await;
    let child_names: Vec<String> = children.iter().map(|b| b.name_any()).collect();
    let child_uids: BTreeSet<String> = children.iter().filter_map(|b| b.uid()).collect();
    assert!(
        child_uids.len() >= 2,
        "need ≥2 produced children to prove a clean multi-child drain; got {child_names:?}"
    );

    let kopia_before =
        observed_snapshot_count(&client, "e2e-polcasc-simul-verify-1", POLCASC_SIMUL_SUBPATH).await;
    assert!(
        kopia_before >= 2,
        "kopia should hold ≥2 snapshots before the simultaneous delete; got {kopia_before}"
    );

    // Delete the schedule and the policy back-to-back (schedule then policy): both
    // cascades (Retain) now race over the SAME children — neither may wedge a finalizer.
    schedules
        .delete(SCHED, &DeleteParams::default())
        .await
        .expect("delete the SnapshotSchedule");
    policies
        .delete(POLICY, &DeleteParams::default())
        .await
        .expect("delete the SnapshotPolicy");

    // Every child drains (no stuck finalizer), and BOTH parents are gone.
    let names_ref: Vec<&str> = child_names.iter().map(String::as_str).collect();
    wait_all_drained(&backups, &names_ref, Duration::from_secs(300)).await;
    wait_until(
        "the schedule and policy CRs are both released",
        Duration::from_secs(120),
        poll_interval(),
        || async {
            let sched_gone = schedules.get_opt(SCHED).await?.is_none();
            let pol_gone = policies.get_opt(POLICY).await?.is_none();
            Ok((sched_gone && pol_gone).then_some(()))
        },
    )
    .await
    .expect("both the schedule and policy finalizers must release under the overlapping cascades");

    // NO DeleteSnapshot activity: no batch delete Job ever covered these children, and
    // the kopia data is intact (both cascades were Retain).
    let my_batches = my_batch_jobs(&jobs, &child_uids).await;
    assert!(
        my_batches.is_empty(),
        "a Retain double-cascade must launch NO SnapshotDeleteBatch Job; found {:?}",
        my_batches.iter().map(|j| j.name_any()).collect::<Vec<_>>()
    );
    let kopia_after =
        observed_snapshot_count(&client, "e2e-polcasc-simul-verify-2", POLCASC_SIMUL_SUBPATH).await;
    assert_eq!(
        kopia_after, kopia_before,
        "a simultaneous Retain schedule+policy delete MUST keep all kopia data; before={kopia_before} after={kopia_after}"
    );

    // Cleanup: the retained kopia snapshots may re-catalog as discovered rows; remove
    // any Snapshot CRs before the repo.
    let repo_uid = repos
        .get(REPO)
        .await
        .ok()
        .and_then(|r| r.uid())
        .unwrap_or_default();
    for r in repo_discovered_rows(&backups, &repo_uid).await {
        let _ = backups
            .delete(&r.name_any(), &DeleteParams::default())
            .await;
    }
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
}
