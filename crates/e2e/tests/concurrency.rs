//! Per-repository mover-Job concurrency: the admission gate, end to end.
//!
//! The feature is "a repository's `spec.concurrency.maxConcurrentJobs` bounds how
//! many mover Jobs read/write it at once, restores excepted". Every claim in that
//! sentence is a whole-pipeline claim — CRD field → resolver → gate → Job
//! creation → status condition → the run still finishing — so it gets a
//! whole-pipeline test rather than only unit coverage of the decision table
//! (which `crates/controller/src/pool.rs` already owns exhaustively).
//!
//! **These scenarios need slow movers.** A real backup over the harness's few
//! files finishes in well under a second, so an uninstrumented run has no window
//! in which two Jobs could even overlap — a cap of 1 would "pass" against a
//! controller that ignored the cap entirely. `kopiur_e2e::slow_mover` swaps the
//! operator's mover image for a sleep-wrapper so each backup holds its slot for a
//! window the scenario chooses; see that module for the restore contract. Because
//! the fixture RESHAPES THE RUNNING OPERATOR (a Deployment rollout), and scenario
//! 4 additionally patches the controller's env, this file owns its own CI shard.
//!
//! The six scenarios:
//!
//! 1. `a_repository_cap_serializes_its_backups` — cap 1, two backups: never two
//!    live at once, the loser surfaces `RepositorySlotAvailable=False`, BOTH
//!    still succeed with a real kopia snapshot, and the loser's condition heals
//!    to `True` once it runs (the heal-ordering regression).
//! 2. `a_restore_is_never_queued_behind_backups` — cap 1 with a backup holding
//!    the slot: a Restore gets its Job immediately while a further backup parks,
//!    and the restore completes.
//!    2b. `a_running_restore_holds_the_slot_and_a_backup_queues_behind_it` — the
//!    OTHER direction, and the one the pool cap is actually sold on: a slow
//!    restore holds the only slot, a backup created against it parks with
//!    `WaitingForSlot`, the two pooled Jobs are never both live, and the backup
//!    still succeeds (with its park healed) once the restore finishes.
//! 3. `an_uncapped_repository_never_grows_the_slot_condition` — the default
//!    install must be indistinguishable from a build without the feature.
//! 4. `the_env_backstop_serializes_across_repositories` —
//!    `KOPIUR_MAX_CONCURRENT_JOBS=1` bounds the pool across DIFFERENT
//!    repositories, which no per-repository cap can do.
//! 5. `replace_cancels_the_running_backup_and_its_victim_is_breaker_exempt` —
//!    `concurrencyPolicy: Replace` on an UNCAPPED repository: a genuinely
//!    `Running` run is cancelled by the next slot, the replacement still backs
//!    something up, and the cancellation sails through a mass-deletion breaker
//!    that is DEMONSTRABLY firing at that moment (a decoy wave of manifest-owning
//!    Snapshots is held on the same repository throughout) — the
//!    `pruned-by: replaced-run` operator-prune exemption.
//!
//! Scenario 5 lives here rather than beside the other schedule scenarios
//! because it needs the same slow-mover fixture (a real backup finishes long
//! before the next cron slot, so there would be nothing to replace) and
//! therefore the same shard.

#![cfg(all(unix, feature = "e2e"))]

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::events::v1::Event;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};

use kopiur_api::{Repository, Restore, Snapshot, SnapshotPolicy, SnapshotSchedule};
use kopiur_e2e::slow_mover::{MoverOp, SlowMover, with_slow_mover_config};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, wait, wait_until};

use common::*;

/// Env carrying the CLUSTER-WIDE pooled-mover-Job backstop
/// (`crate::config::MAX_CONCURRENT_JOBS_ENV`). Spelled as a literal rather than
/// imported: the e2e crate does not depend on the controller crate, so a rename
/// there must fail an e2e run loudly instead of silently testing nothing.
const MAX_CONCURRENT_JOBS_ENV: &str = "KOPIUR_MAX_CONCURRENT_JOBS";

/// The condition the gate writes on a queued run.
const SLOT_CONDITION: &str = "RepositorySlotAvailable";

/// How long each slow backup holds its slot. Long enough that a 1s poll cannot
/// miss the queued window, short enough that two serialized runs plus the
/// fixture's two Deployment rollouts fit comfortably inside a shard.
const BACKUP_DELAY: Duration = Duration::from_secs(45);

/// Poll cadence for the "never two live at once" invariant. A real violation
/// would persist for a whole mover's lifetime (tens of seconds), so 1s cannot
/// slip past one.
const INVARIANT_POLL: Duration = Duration::from_secs(1);

// --- shared plumbing ---------------------------------------------------------

/// Whether a mover `Job` is TERMINAL (`Complete`/`Failed` = True). Mirrors the
/// controller's `job_terminal_state`; spelled here for the same reason the env
/// name is.
fn job_is_terminal(job: &Job) -> bool {
    job.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|cs| {
            cs.iter()
                .any(|c| (c.type_ == "Complete" || c.type_ == "Failed") && c.status == "True")
        })
}

/// How many of `names` currently have a LIVE (existing, non-terminal) mover Job.
///
/// A backup's mover Job is named after its `Snapshot` and a direct restore's
/// after its `Restore`, so the CR name is the Job name — no label arithmetic,
/// and nothing another scenario creates can be miscounted as ours.
async fn live_jobs(jobs: &Api<Job>, names: &[&str]) -> Result<usize, kube::Error> {
    let mut live = 0;
    for n in names {
        if let Some(j) = jobs.get_opt(n).await?
            && !job_is_terminal(&j)
        {
            live += 1;
        }
    }
    Ok(live)
}

/// The `status` of one of a CR's conditions (`None` = the condition is absent, a
/// distinction several scenarios here turn on), keeping an API error DISTINCT
/// from an absent condition.
///
/// The distinction matters in exactly one direction. A never-`True` invariant
/// read through the lossy [`condition_status`] can only be WEAKENED by a
/// transient apiserver blip — a missed poll, and the states involved persist for
/// many polls. A must-be-`True` invariant read the same way would see the blip as
/// a violation and fail the run for it, so those callers use this form and skip
/// the poll on `Err` instead.
async fn condition_status_checked<K>(
    api: &Api<K>,
    name: &str,
    type_: &str,
) -> Result<Option<String>, kube::Error>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + serde::Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    let Some(obj) = api.get_opt(name).await? else {
        return Ok(None);
    };
    let status = serde_json::to_value(&obj)
        .ok()
        .and_then(|v| v.get("status").cloned())
        .unwrap_or(serde_json::Value::Null);
    Ok(status
        .get("conditions")
        .and_then(|c| c.as_array())
        .and_then(|a| {
            a.iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some(type_))
        })
        .and_then(|c| c.get("status").and_then(|s| s.as_str()))
        .map(str::to_string))
}

/// [`condition_status_checked`] with an API error folded into "absent" — the
/// best-effort form the never-`True` invariants and the uncapped scenario use.
async fn condition_status<K>(api: &Api<K>, name: &str, type_: &str) -> Option<String>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + serde::Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    condition_status_checked(api, name, type_).await.ok()?
}

/// [`condition_status`] for the gate's own condition — the one the first three
/// scenarios read on nearly every poll.
async fn slot_condition_status<K>(api: &Api<K>, name: &str) -> Option<String>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + serde::Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    condition_status(api, name, SLOT_CONDITION).await
}

/// Create a namespaced filesystem `Repository` over `subpath` with an optional
/// per-repository pool cap, and wait for it to be Ready.
async fn ensure_capped_repo(client: &Client, name: &str, subpath: &str, cap: Option<u32>) {
    ensure_repo(client, subpath).await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let extra = match cap {
        Some(n) => serde_json::json!({ "concurrency": { "maxConcurrentJobs": n } }),
        None => serde_json::json!({}),
    };
    create_idempotent(
        &repos,
        &cr(repository_json(name, subpath, extra)),
        "create Repository",
    )
    .await;
    // Read back and assert the cap LANDED. A field pruned by a stale CRD schema
    // (or a `{"spec": ...}` wrapper swallowed by merge_spec) would leave this
    // scenario proving nothing, and it would fail minutes later as "the gate
    // does not work" rather than "the field never reached the CR".
    let created = repos.get(name).await.expect("read back the Repository");
    let got = created
        .spec
        .concurrency
        .as_ref()
        .and_then(|c| c.max_concurrent_jobs);
    assert_eq!(
        got, cap,
        "spec.concurrency.maxConcurrentJobs must land on {name}, got {got:?}"
    );
    wait_phase(&repos, name, "Ready").await.expect("repo Ready");
}

/// Create a `SnapshotPolicy` over `repo` (idempotent).
async fn ensure_policy(client: &Client, policy: &str, repo: &str) {
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &policies,
        &cr(snapshot_policy_json(
            E2E_NAMESPACE,
            policy,
            "Repository",
            repo,
            serde_json::json!({ "identity": { "username": policy, "hostname": "e2e" } }),
        )),
        "create SnapshotPolicy",
    )
    .await;
}

/// Create a `Snapshot` for `policy` (idempotent).
async fn create_backup(backups: &Api<Snapshot>, name: &str, policy: &str) {
    create_idempotent(
        backups,
        &cr(snapshot_json(
            E2E_NAMESPACE,
            name,
            policy,
            serde_json::json!({}),
        )),
        "create Snapshot",
    )
    .await;
}

/// Assert a `Snapshot` succeeded AND owns a real kopia manifest — the difference
/// between "the queue drained" and "the queued run actually backed anything up".
async fn assert_real_snapshot(backups: &Api<Snapshot>, name: &str) -> anyhow::Result<()> {
    wait_phase(backups, name, "Succeeded").await?;
    let s = status_json(backups, name).await;
    let id = s["snapshot"]["kopiaSnapshotID"].as_str().unwrap_or("");
    anyhow::ensure!(
        !id.is_empty(),
        "{name} must own a real kopia snapshot after being admitted, got {:?}",
        s["snapshot"]
    );
    Ok(())
}

/// Patch the controller Deployment's [`MAX_CONCURRENT_JOBS_ENV`] and wait for the
/// rollout, so the only running controller pod carries `value`. `"0"` restores
/// the chart default (uncapped). Same discipline as
/// `mass_deletion::set_delete_job_cap`: strategic-merge on the container by name
/// so the env entry the chart always renders is UPDATED rather than duplicated.
async fn set_global_job_cap(client: &Client, value: &str) -> anyhow::Result<()> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    api.patch(
        kopiur_e2e::consts::CONTROLLER_DEPLOYMENT,
        &PatchParams::default(),
        &Patch::Strategic(serde_json::json!({
            "spec": { "template": { "spec": { "containers": [
                { "name": kopiur_e2e::consts::CONTROLLER_CONTAINER,
                  "env": [ { "name": MAX_CONCURRENT_JOBS_ENV, "value": value } ] }
            ]}}}
        })),
    )
    .await?;
    wait::deployment_ready(
        client,
        E2E_NAMESPACE,
        kopiur_e2e::consts::CONTROLLER_DEPLOYMENT,
    )
    .await
}

/// Best-effort teardown so a rerun of the shard starts clean.
async fn cleanup(client: &Client, repos: &[&str], policies: &[&str], backups: &[&str]) {
    let b: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    for n in backups {
        let _ = b.delete(n, &DeleteParams::default()).await;
    }
    let p: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    for n in policies {
        let _ = p.delete(n, &DeleteParams::default()).await;
    }
    let r: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    for n in repos {
        let _ = r.delete(n, &DeleteParams::default()).await;
    }
}

// --- 1. the per-repository cap serializes backups ------------------------------

const CAP_SUBPATH: &str = "conc-cap";

/// `maxConcurrentJobs: 1` with two backups queued against one repository.
///
/// Four claims, and the invariant is the load-bearing one:
///
/// * **Never two live backup Jobs at once.** Asserted on every 1s poll for the
///   whole drain, so it holds regardless of how the two runs happen to interleave
///   — a timing-robust proof, unlike "the second one started later".
/// * The queued run surfaces `RepositorySlotAvailable=False`/`WaitingForSlot`,
///   so a human can see WHY it is Pending rather than guessing.
/// * **Both** reach `Succeeded` with a real `kopiaSnapshotID`. A cap that
///   serialized by dropping the loser would pass the invariant and be a
///   data-loss bug.
/// * The loser's condition ends `True`. That is the heal-ordering regression:
///   the flip is folded into the Job-creation status write, and any later
///   condition writer seeding from the stale reflector copy would resurrect the
///   `False` onto a run whose Job is already going.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn a_repository_cap_serializes_its_backups() -> anyhow::Result<()> {
    let Some(world) = World::connect().await else {
        return Ok(());
    };
    // MUST precede the slow-mover enable: the fixture writes its delay knobs into
    // the filesystem credentials Secret and fails fast if it does not exist yet.
    world.ensure(&[Need::Filesystem]).await?;
    let client: Client = world.client().clone();

    const REPO: &str = "e2e-conc-cap-repo";
    const POLICY: &str = "e2e-conc-cap-pol";
    const FIRST: &str = "e2e-conc-cap-1";
    const SECOND: &str = "e2e-conc-cap-2";

    ensure_capped_repo(&client, REPO, CAP_SUBPATH, Some(1)).await;
    ensure_policy(&client, POLICY, REPO).await;

    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // Only backups crawl: bootstrap and any restore stay full speed, so the
    // window under test is exactly the one the cap governs.
    let config = SlowMover::new(BACKUP_DELAY).ops(&[MoverOp::Snapshot]);
    let result = with_slow_mover_config(&world, config, || async {
        create_backup(&backups, FIRST, POLICY).await;
        create_backup(&backups, SECOND, POLICY).await;

        let mut saw_queued: BTreeSet<String> = BTreeSet::new();
        let deadline = Instant::now() + Duration::from_secs(420);
        loop {
            let live = live_jobs(&jobs, &[FIRST, SECOND]).await?;
            anyhow::ensure!(
                live <= 1,
                "maxConcurrentJobs=1 violated: {live} live backup Jobs against one repository"
            );
            for n in [FIRST, SECOND] {
                if slot_condition_status(&backups, n).await.as_deref() == Some("False") {
                    saw_queued.insert(n.to_string());
                }
            }
            let phases: Vec<String> = {
                let mut v = Vec::new();
                for n in [FIRST, SECOND] {
                    v.push(
                        status_json(&backups, n).await["phase"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                    );
                }
                v
            };
            if phases.iter().all(|p| p == "Succeeded") {
                break;
            }
            anyhow::ensure!(
                !phases.iter().any(|p| p == "Failed"),
                "a queued backup must be DEFERRED, never failed: phases={phases:?}"
            );
            anyhow::ensure!(
                Instant::now() < deadline,
                "the serialized backups did not both drain in time: phases={phases:?}"
            );
            tokio::time::sleep(INVARIANT_POLL).await;
        }

        anyhow::ensure!(
            !saw_queued.is_empty(),
            "with cap=1 and two simultaneous backups, one MUST have surfaced \
             {SLOT_CONDITION}=False — if neither did, the gate never ran"
        );
        // The reason is what a human reads in `kubectl describe`.
        for n in &saw_queued {
            let s = status_json(&backups, n).await;
            let cond = s["conditions"]
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find(|c| c["type"].as_str() == Some(SLOT_CONDITION))
                })
                .cloned()
                .unwrap_or_default();
            // Post-drain it must have HEALED — the same condition, now True.
            anyhow::ensure!(
                cond["status"].as_str() == Some("True")
                    && cond["reason"].as_str() == Some("SlotAcquired"),
                "the queued run's condition must heal to True/SlotAcquired once it \
                 launches (the heal-ordering regression), got {cond}"
            );
        }
        Ok(())
    })
    .await;

    // Both runs must have really backed something up — asserted OUTSIDE the slow
    // window so a fixture-restore failure cannot be mistaken for a backup fault.
    result?;
    assert_real_snapshot(&backups, FIRST).await?;
    assert_real_snapshot(&backups, SECOND).await?;

    cleanup(&client, &[REPO], &[POLICY], &[FIRST, SECOND]).await;
    Ok(())
}

// --- 2. a restore is never queued behind backups -------------------------------

const RESTORE_SUBPATH: &str = "conc-restore";

/// The guarantee the feature is sold on: a recovery does not wait in line.
///
/// With `maxConcurrentJobs: 1` and a slow backup holding the only slot, a
/// `Restore` created afterwards gets its mover Job **immediately** — while a
/// further backup created at the same moment parks. Both halves matter: the
/// restore proves restores are exempt, and the parked backup proves the pool
/// really was full at that instant (otherwise the restore's admission would be
/// vacuous).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn a_restore_is_never_queued_behind_backups() -> anyhow::Result<()> {
    let Some(world) = World::connect().await else {
        return Ok(());
    };
    world.ensure(&[Need::Filesystem]).await?;
    let client: Client = world.client().clone();

    const REPO: &str = "e2e-conc-rst-repo";
    const POLICY: &str = "e2e-conc-rst-pol";
    const SEED: &str = "e2e-conc-rst-seed";
    const HOLDER: &str = "e2e-conc-rst-hold";
    const PARKED: &str = "e2e-conc-rst-park";
    const RESTORE: &str = "e2e-conc-rst-restore";

    ensure_capped_repo(&client, REPO, RESTORE_SUBPATH, Some(1)).await;
    ensure_policy(&client, POLICY, REPO).await;

    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // Seed a real snapshot to restore FROM, at full speed (outside the fixture).
    create_backup(&backups, SEED, POLICY).await;
    assert_real_snapshot(&backups, SEED).await?;

    let config = SlowMover::new(BACKUP_DELAY).ops(&[MoverOp::Snapshot]);
    let result = with_slow_mover_config(&world, config, || async {
        // Fill the single slot with a slow backup.
        create_backup(&backups, HOLDER, POLICY).await;
        wait_until(
            &format!("{HOLDER} holds the only slot"),
            Duration::from_secs(180),
            INVARIANT_POLL,
            || async { Ok((live_jobs(&jobs, &[HOLDER]).await? == 1).then_some(())) },
        )
        .await?;

        // Now ask for both a restore and another backup.
        create_backup(&backups, PARKED, POLICY).await;
        restores
            .create(
                &PostParams::default(),
                &cr(serde_json::json!({
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "Restore",
                    "metadata": { "name": RESTORE, "namespace": E2E_NAMESPACE },
                    "spec": {
                        "repository": { "kind": "Repository", "name": REPO },
                        "source": { "snapshotRef": { "name": SEED } },
                        "target": { "pvc": {
                            "name": format!("{RESTORE}-dst"),
                            "capacity": "1Gi",
                            "accessModes": ["ReadWriteOnce"],
                        }},
                    }
                })),
            )
            .await?;

        // ORDER MATTERS, and it is the opposite of the narrative order.
        //
        // `PARKED` being queued is a TRANSIENT state — it lasts only until the
        // holder's 45s mover finishes. `wait_until` cannot observe a state that
        // has already passed, so polling for it AFTER a slow restore-Job wait
        // would, on a loaded runner, start looking only once PARKED had already
        // been admitted: it would burn its whole window and fail as "the backup
        // never parked" when in fact the gate worked perfectly. Poll for the
        // ephemeral fact FIRST, while the holder is still known to be running.
        wait_until(
            &format!("{PARKED} queued behind the full pool"),
            Duration::from_secs(180),
            INVARIANT_POLL,
            || async {
                Ok(
                    (slot_condition_status(&backups, PARKED).await.as_deref() == Some("False"))
                        .then_some(()),
                )
            },
        )
        .await?;

        // The restore's Job appears even though the pool is full — and the park
        // just observed is what makes that non-vacuous. This half is safe to
        // check second: a Job, once created, PERSISTS to its TTL, so unlike the
        // park there is no window to miss. Generous window because the restore
        // still resolves its source and stages a target PVC first; none of that
        // is what this asserts — the claim is that it is never HELD.
        wait_until(
            &format!("{RESTORE} mover Job created while the pool is full"),
            Duration::from_secs(180),
            INVARIANT_POLL,
            || async { Ok(jobs.get_opt(RESTORE).await?.map(|_| ())) },
        )
        .await?;
        anyhow::ensure!(
            slot_condition_status(&restores, RESTORE).await.as_deref() != Some("False"),
            "a Restore must NEVER carry {SLOT_CONDITION}=False — restores are \
             admitted at and over the cap"
        );

        wait_phase(&restores, RESTORE, "Completed").await?;
        Ok(())
    })
    .await;

    result?;
    // The parked backup still drains once the holder finishes.
    assert_real_snapshot(&backups, PARKED).await?;

    let _ = restores.delete(RESTORE, &DeleteParams::default()).await;
    cleanup(&client, &[REPO], &[POLICY], &[SEED, HOLDER, PARKED]).await;
    Ok(())
}

// --- 2b. the other direction: a restore HOLDS the slot -------------------------

const COUNTED_SUBPATH: &str = "conc-counted";

/// The half of the restore contract scenario 2 never exercises: a restore
/// **occupies** the slot it was never held from.
///
/// Scenario 2 proves a restore is not queued *behind* a backup. This proves a
/// backup is queued behind a *restore* — which is the claim `maxConcurrentJobs`
/// is actually sold on ("a restore displaces backups rather than adding to
/// them"), and the claim that broke when the restore path took no admission
/// reservation at all.
///
/// With `maxConcurrentJobs: 1` and the slow-mover fixture delaying
/// [`MoverOp::Restore`], a `Restore` holds the only slot for a long, observable
/// window. Four claims:
///
/// * **Never two live pooled Jobs at once**, polled every second for the whole
///   window. A restore that did not count would let the backup start beside it,
///   and this is what would catch it.
/// * The backup surfaces `RepositorySlotAvailable=False`/`WaitingForSlot`, so
///   the queueing is visible rather than inferred from timing.
/// * The restore reaches `Completed` — a cap must not have slowed the recovery
///   down, only the work behind it.
/// * The backup then **succeeds with a real kopia snapshot** and its condition
///   heals to `True`. A cap that serialized by dropping the loser would satisfy
///   the invariant and be a data-loss bug.
///
/// **Why the restore is started FIRST rather than simultaneously with the
/// backup.** Admission is first-come: whichever run reaches the gate first takes
/// the slot, and a restore that lost that race is — correctly, by design —
/// admitted over the cap beside the backup. Racing them here would test
/// scheduler luck, not the invariant. The genuinely simultaneous, pre-Job
/// window is a sub-millisecond one that no cluster fixture can hit reliably;
/// `crates/controller/src/pool.rs` owns it with a barriered ledger test
/// (`racing_backups_all_park_on_an_in_flight_restores_reservation`), which is
/// exactly the split this file's header describes.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn a_running_restore_holds_the_slot_and_a_backup_queues_behind_it() -> anyhow::Result<()> {
    let Some(world) = World::connect().await else {
        return Ok(());
    };
    world.ensure(&[Need::Filesystem]).await?;
    let client: Client = world.client().clone();

    const REPO: &str = "e2e-conc-cnt-repo";
    const POLICY: &str = "e2e-conc-cnt-pol";
    const SEED: &str = "e2e-conc-cnt-seed";
    const QUEUED: &str = "e2e-conc-cnt-queued";
    const RESTORE: &str = "e2e-conc-cnt-restore";

    ensure_capped_repo(&client, REPO, COUNTED_SUBPATH, Some(1)).await;
    ensure_policy(&client, POLICY, REPO).await;

    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // Seed a real snapshot to restore FROM, at full speed (outside the fixture).
    create_backup(&backups, SEED, POLICY).await;
    assert_real_snapshot(&backups, SEED).await?;

    // BOTH ops are slowed: the restore so it holds the slot long enough to
    // observe, the backup so that if it ever did start beside the restore its
    // Job would still be live on the next poll instead of finishing between two
    // of them and hiding the violation.
    let config = SlowMover::new(BACKUP_DELAY).ops(&[MoverOp::Restore, MoverOp::Snapshot]);
    let result = with_slow_mover_config(&world, config, || async {
        restores
            .create(
                &PostParams::default(),
                &cr(serde_json::json!({
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "Restore",
                    "metadata": { "name": RESTORE, "namespace": E2E_NAMESPACE },
                    "spec": {
                        "repository": { "kind": "Repository", "name": REPO },
                        "source": { "snapshotRef": { "name": SEED } },
                        "target": { "pvc": {
                            "name": format!("{RESTORE}-dst"),
                            "capacity": "1Gi",
                            "accessModes": ["ReadWriteOnce"],
                        }},
                    }
                })),
            )
            .await?;

        // The restore takes the only slot. Generous window: it resolves its
        // source and stages a target PVC before its mover Job exists, and none
        // of that is what this scenario asserts.
        wait_until(
            &format!("{RESTORE} holds the only slot"),
            Duration::from_secs(180),
            INVARIANT_POLL,
            || async { Ok((live_jobs(&jobs, &[RESTORE]).await? == 1).then_some(())) },
        )
        .await?;

        // Only now ask for a backup, so its gate runs against a pool the restore
        // is demonstrably occupying.
        create_backup(&backups, QUEUED, POLICY).await;

        // THE INVARIANT, and the park, in ONE explicit loop.
        //
        // Not `wait_until`: that helper SWALLOWS a poll error and retries to its
        // deadline, so an `ensure!` violation inside it would be reported as a
        // timeout rather than as the invariant breach it is. Scenario 1 owns the
        // same shape for the same reason.
        //
        // The park is recorded as it is SEEN rather than waited for afterwards:
        // it is an ephemeral state that ends when the restore's mover finishes,
        // and a second pass looking for it could start after it had already
        // passed — the `a_restore_is_never_queued_behind_backups` ordering
        // lesson.
        let mut saw_queued = false;
        let deadline = Instant::now() + Duration::from_secs(420);
        loop {
            let live = live_jobs(&jobs, &[RESTORE, QUEUED]).await?;
            anyhow::ensure!(
                live <= 1,
                "maxConcurrentJobs=1 ran {live} pooled Jobs at once: a restore and \
                 a backup were both admitted against one repository"
            );
            saw_queued |= slot_condition_status(&backups, QUEUED).await.as_deref() == Some("False");
            let restore_phase = status_json(&restores, RESTORE).await["phase"]
                .as_str()
                .unwrap_or("")
                .to_string();
            anyhow::ensure!(
                restore_phase != "Failed",
                "the restore must not fail: a cap may queue work behind a recovery, \
                 never break it"
            );
            if restore_phase == "Completed" {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "the restore never completed: phase={restore_phase:?}"
            );
            tokio::time::sleep(INVARIANT_POLL).await;
        }
        anyhow::ensure!(
            saw_queued,
            "a backup created against a repository whose ONLY slot is held by a \
             running restore MUST surface {SLOT_CONDITION}=False — if it never \
             did, the restore was not counted and the gate never ran"
        );
        anyhow::ensure!(
            slot_condition_status(&restores, RESTORE).await.as_deref() != Some("False"),
            "a Restore must NEVER carry {SLOT_CONDITION}=False — restores are \
             admitted at and over the cap"
        );
        Ok(())
    })
    .await;

    result?;
    // The queued backup drains once the restore releases the slot, really backs
    // something up, and its park heals rather than sticking to a running run.
    assert_real_snapshot(&backups, QUEUED).await?;
    // Polled, not read once: the heal rides the Job-creation status patch and the
    // mover's own terminal patch lands moments later, so a single read taken
    // between the two would be a coin flip rather than a claim. It must REACH
    // `True` — a stuck `False` (the heal-ordering regression) still fails.
    wait_until(
        &format!("{QUEUED} heals {SLOT_CONDITION} once it is admitted"),
        Duration::from_secs(120),
        INVARIANT_POLL,
        || async {
            Ok(
                (condition_status_checked(&backups, QUEUED, SLOT_CONDITION).await?
                    == Some("True".into()))
                .then_some(()),
            )
        },
    )
    .await?;

    let _ = restores.delete(RESTORE, &DeleteParams::default()).await;
    cleanup(&client, &[REPO], &[POLICY], &[SEED, QUEUED]).await;
    Ok(())
}

// --- 3. the uncapped default is unchanged behavior -----------------------------

const UNCAPPED_SUBPATH: &str = "conc-uncapped";

/// The default install must be indistinguishable from a build without the gate.
///
/// A repository with no `spec.concurrency` at all: two simultaneous backups both
/// run, and NEITHER ever grows a `RepositorySlotAvailable` condition — absent,
/// not merely `True`. The distinction is the point: a gate that wrote the
/// condition unconditionally would add a status field (and a
/// `resourceVersion` bump, and a GitOps diff) to every backup in every existing
/// install that never asked for a cap.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn an_uncapped_repository_never_grows_the_slot_condition() -> anyhow::Result<()> {
    let Some(world) = World::connect().await else {
        return Ok(());
    };
    world.ensure(&[Need::Filesystem]).await?;
    let client: Client = world.client().clone();

    const REPO: &str = "e2e-conc-unc-repo";
    const POLICY: &str = "e2e-conc-unc-pol";
    const FIRST: &str = "e2e-conc-unc-1";
    const SECOND: &str = "e2e-conc-unc-2";

    ensure_capped_repo(&client, REPO, UNCAPPED_SUBPATH, None).await;
    ensure_policy(&client, POLICY, REPO).await;

    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // Slow movers here too — without them the two runs would never overlap and
    // "no condition appeared" would prove nothing.
    let config = SlowMover::new(BACKUP_DELAY).ops(&[MoverOp::Snapshot]);
    let result = with_slow_mover_config(&world, config, || async {
        create_backup(&backups, FIRST, POLICY).await;
        create_backup(&backups, SECOND, POLICY).await;

        let mut saw_overlap = false;
        let deadline = Instant::now() + Duration::from_secs(420);
        loop {
            if live_jobs(&jobs, &[FIRST, SECOND]).await? == 2 {
                saw_overlap = true;
            }
            for n in [FIRST, SECOND] {
                anyhow::ensure!(
                    slot_condition_status(&backups, n).await.is_none(),
                    "an UNCAPPED repository must never write {SLOT_CONDITION} at all \
                     (absent, not True) — {n} grew one"
                );
            }
            let mut all_done = true;
            for n in [FIRST, SECOND] {
                let phase = status_json(&backups, n).await["phase"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                anyhow::ensure!(phase != "Failed", "{n} failed on an uncapped repository");
                all_done &= phase == "Succeeded";
            }
            if all_done {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "uncapped backups did not both finish in time"
            );
            tokio::time::sleep(INVARIANT_POLL).await;
        }
        anyhow::ensure!(
            saw_overlap,
            "the two uncapped backups never overlapped, so this scenario proved \
             nothing — the slow-mover window is too short or the fixture did not apply"
        );
        Ok(())
    })
    .await;

    result?;
    assert_real_snapshot(&backups, FIRST).await?;
    assert_real_snapshot(&backups, SECOND).await?;
    // Final state, after every follow-up heal pass: still no condition.
    for n in [FIRST, SECOND] {
        anyhow::ensure!(
            slot_condition_status(&backups, n).await.is_none(),
            "{n} grew {SLOT_CONDITION} after completing on an uncapped repository"
        );
    }

    cleanup(&client, &[REPO], &[POLICY], &[FIRST, SECOND]).await;
    Ok(())
}

// --- 4. the cluster-wide env backstop ------------------------------------------

const ENV_SUBPATH_A: &str = "conc-env-a";
const ENV_SUBPATH_B: &str = "conc-env-b";

/// `KOPIUR_MAX_CONCURRENT_JOBS=1` bounds the pool across DIFFERENT repositories
/// — the thing no per-repository cap can do, and the reason the backstop exists.
///
/// Two repositories, neither with a cap of its own, one backup each: with the
/// env set they must still serialize. The Deployment mutation follows the
/// `set_delete_job_cap` discipline (patch + rollout wait, restored on EVERY exit
/// path), and everything inside the window `?`-propagates so a failure can never
/// skip the restore and leave the cap on for the rest of the run.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn the_env_backstop_serializes_across_repositories() -> anyhow::Result<()> {
    let Some(world) = World::connect().await else {
        return Ok(());
    };
    world.ensure(&[Need::Filesystem]).await?;
    let client: Client = world.client().clone();

    const REPO_A: &str = "e2e-conc-env-a-repo";
    const REPO_B: &str = "e2e-conc-env-b-repo";
    const POLICY_A: &str = "e2e-conc-env-a-pol";
    const POLICY_B: &str = "e2e-conc-env-b-pol";
    const BACKUP_A: &str = "e2e-conc-env-a-1";
    const BACKUP_B: &str = "e2e-conc-env-b-1";

    // Provision both repositories UNCAPPED and outside the mutation window, so
    // the only thing serializing them can be the env backstop.
    ensure_capped_repo(&client, REPO_A, ENV_SUBPATH_A, None).await;
    ensure_capped_repo(&client, REPO_B, ENV_SUBPATH_B, None).await;
    ensure_policy(&client, POLICY_A, REPO_A).await;
    ensure_policy(&client, POLICY_B, REPO_B).await;

    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // Guard the SET as `mass_deletion` does: if the patch landed but the rollout
    // wait then errored, the cap is already "1" and the restore below MUST run.
    let set_result = set_global_job_cap(&client, "1").await;

    let body: anyhow::Result<()> = async {
        if set_result.is_err() {
            return Ok(());
        }
        let config = SlowMover::new(BACKUP_DELAY).ops(&[MoverOp::Snapshot]);
        with_slow_mover_config(&world, config, || async {
            create_backup(&backups, BACKUP_A, POLICY_A).await;
            create_backup(&backups, BACKUP_B, POLICY_B).await;

            let mut saw_queued = false;
            let deadline = Instant::now() + Duration::from_secs(420);
            loop {
                let live = live_jobs(&jobs, &[BACKUP_A, BACKUP_B]).await?;
                anyhow::ensure!(
                    live <= 1,
                    "KOPIUR_MAX_CONCURRENT_JOBS=1 violated: {live} live mover Jobs \
                     across two DIFFERENT repositories"
                );
                for n in [BACKUP_A, BACKUP_B] {
                    if slot_condition_status(&backups, n).await.as_deref() == Some("False") {
                        saw_queued = true;
                    }
                }
                let mut all_done = true;
                for n in [BACKUP_A, BACKUP_B] {
                    let phase = status_json(&backups, n).await["phase"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    anyhow::ensure!(phase != "Failed", "{n} failed under the env backstop");
                    all_done &= phase == "Succeeded";
                }
                if all_done {
                    break;
                }
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "backups under the env backstop did not both drain in time"
                );
                tokio::time::sleep(INVARIANT_POLL).await;
            }
            anyhow::ensure!(
                saw_queued,
                "one of the two cross-repository backups MUST have surfaced \
                 {SLOT_CONDITION}=False under the backstop"
            );
            Ok(())
        })
        .await
    }
    .await;

    // ALWAYS restore the chart default (uncapped) + rollout wait.
    let restore = set_global_job_cap(&client, "0").await;

    set_result?;
    body?;
    restore?;

    assert_real_snapshot(&backups, BACKUP_A).await?;
    assert_real_snapshot(&backups, BACKUP_B).await?;
    cleanup(
        &client,
        &[REPO_A, REPO_B],
        &[POLICY_A, POLICY_B],
        &[BACKUP_A, BACKUP_B],
    )
    .await;
    Ok(())
}

// --- 5. concurrencyPolicy: Replace cancels the in-flight run --------------------

const REPLACE_SUBPATH: &str = "conc-replace";

/// The label a `SnapshotSchedule` stamps on every `Snapshot` it produces.
/// Spelled as a literal for the same reason [`MAX_CONCURRENT_JOBS_ENV`] is: the
/// e2e crate does not depend on the controller, so a rename there must fail an
/// e2e run loudly rather than silently select nothing.
const SCHEDULE_LABEL: &str = "kopiur.home-operations.com/schedule";

/// The `Repository` condition the mass-deletion breaker raises once pending
/// EXTERNAL destructive `Snapshot` deletions reach `deletionProtection.threshold`.
const MASS_DELETION_HELD_CONDITION: &str = "MassDeletionHeld";

/// The per-`Snapshot` condition a HELD deletion carries while it waits for an
/// acknowledgement.
const DELETION_HELD_CONDITION: &str = "DeletionHeld";

/// The `reason` on [`DELETION_HELD_CONDITION`] when the breaker is what is
/// holding (as opposed to any other future hold).
const MASS_DELETION_BREAKER_REASON: &str = "MassDeletionBreaker";

/// The condition `Replace` raises when it HOLDS a slot instead of replacing —
/// the run it would cancel is itself parked behind the repository's mover-Job
/// pool cap, so cancelling it would free no capacity. This scenario deliberately
/// runs an UNCAPPED repository so that path is unreachable, and asserts the
/// condition never appears: a regression that routed the fire through the hold
/// would otherwise be free to masquerade as a pass.
const REPLACEMENT_HELD_CONDITION: &str = "ReplacementHeld";

/// The `Event` reason `Replace` publishes on the schedule when it cancels runs.
const REPLACED_EVENT_REASON: &str = "ReplacedActiveRun";

/// How long a backup mover holds its slot in the Replace scenario.
///
/// The whole scenario turns on ONE inequality: the run observed `Running` cannot
/// possibly finish before the next cron slot arrives. An every-minute cron is
/// the finest grain there is, so slots are 60s apart, and the worst case is
/// observing the run at the very instant its mover started — anything over 60s
/// makes the replacement inevitable rather than lucky. 150s leaves ~90s of
/// margin for a loaded kind node, and is deliberately not "sleep and hope":
/// nothing here waits out a duration, the margin only guarantees that the state
/// the waits poll for actually comes to exist.
const REPLACE_DELAY: Duration = Duration::from_secs(150);

/// The three `Snapshot`s whose bulk deletion TRIPS the mass-deletion breaker for
/// the Replace scenario's repository, so the victim's exemption is measured
/// against a breaker that is demonstrably firing rather than one that could
/// never have fired. See the scenario's doc comment for why a `Replace` victim
/// cannot trip it itself.
const DECOYS: [&str; 3] = [
    "e2e-conc-repl-decoy-1",
    "e2e-conc-repl-decoy-2",
    "e2e-conc-repl-decoy-3",
];

/// Names of this schedule's produced `Snapshot` children, by the label the
/// schedule stamps (not a name prefix — the label is the population the
/// controller's own concurrency gate reads).
async fn schedule_children(
    backups: &Api<Snapshot>,
    schedule: &str,
) -> Result<Vec<String>, kube::Error> {
    let lp = ListParams::default().labels(&format!("{SCHEDULE_LABEL}={schedule}"));
    Ok(backups
        .list(&lp)
        .await?
        .items
        .into_iter()
        .filter_map(|s| s.metadata.name)
        .collect())
}

/// Page the namespace's `Event`s in bounded chunks and return the first one
/// matching `pred`.
///
/// Bounded rather than one unbounded LIST because this runs on every poll and
/// the e2e namespace accumulates Events for the whole shard. Paged rather than
/// simply capped so a busy namespace cannot push the Event we need off the end
/// of the first page and turn a real pass into a timeout. No field selector:
/// which fields `events.k8s.io/v1` makes selectable is version-dependent, and an
/// unsupported one is a 400 that `wait_until` would swallow as "not ready yet"
/// — a silent full-timeout failure.
async fn find_event<P>(events: &Api<Event>, mut pred: P) -> Result<Option<Event>, kube::Error>
where
    P: FnMut(&Event) -> bool,
{
    let mut token: Option<String> = None;
    loop {
        let mut lp = ListParams::default().limit(EVENT_PAGE_SIZE);
        if let Some(t) = &token {
            lp = lp.continue_token(t);
        }
        let page = events.list(&lp).await?;
        if let Some(found) = page.items.into_iter().find(&mut pred) {
            return Ok(Some(found));
        }
        match page.metadata.continue_.filter(|c| !c.is_empty()) {
            Some(c) => token = Some(c),
            None => return Ok(None),
        }
    }
}

/// Per-request Event page size for [`find_event`].
const EVENT_PAGE_SIZE: u32 = 500;

/// Set the repository's mass-deletion breaker threshold and verify it LANDED.
///
/// Read-back for the same reason `ensure_capped_repo` reads its cap back: a
/// pruned or mis-shaped field would leave the breaker at its default of 10,
/// where the decoy wave below could not trip it — and the scenario's exemption
/// proof would degrade back into proving nothing.
async fn set_deletion_threshold(
    repos: &Api<Repository>,
    name: &str,
    threshold: u32,
) -> anyhow::Result<()> {
    repos
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "spec": { "deletionProtection": { "threshold": threshold } }
            })),
        )
        .await?;
    let got = repos
        .get(name)
        .await?
        .spec
        .deletion_protection
        .as_ref()
        .and_then(|d| d.threshold);
    anyhow::ensure!(
        got == Some(threshold),
        "spec.deletionProtection.threshold must land on {name} as {threshold}, got {got:?}"
    );
    Ok(())
}

/// Delete `schedule`, every `Snapshot` it produced, and the named `decoys`, then
/// wait until all of them are gone.
///
/// The `e2e` nextest profile retries a failed test IN PLACE, and a schedule left
/// behind by a failed attempt is left SUSPENDED by this scenario's own body — a
/// retry that merely tolerated `AlreadyExists` would then wait forever for a
/// slot that can never fire. So the schedule is reset rather than reused, and the
/// decoys are re-seeded fresh (a leftover decoy still terminating from a previous
/// attempt would keep the wave held and desynchronize this attempt's arming).
///
/// The caller MUST disarm the breaker first (threshold `0`): these are EXTERNAL
/// destructive deletes, which are exactly what an armed breaker holds.
async fn reset_replace_fixtures(
    schedules: &Api<SnapshotSchedule>,
    backups: &Api<Snapshot>,
    schedule: &str,
    decoys: &[&str],
) -> anyhow::Result<()> {
    let _ = schedules.delete(schedule, &DeleteParams::default()).await;
    for name in schedule_children(backups, schedule).await? {
        let _ = backups.delete(&name, &DeleteParams::default()).await;
    }
    for name in decoys {
        let _ = backups.delete(name, &DeleteParams::default()).await;
    }
    wait_until(
        &format!("leftovers of {schedule} (and its decoys) are gone"),
        Duration::from_secs(300),
        INVARIANT_POLL,
        || async {
            if schedules.get_opt(schedule).await?.is_some() {
                return Ok(None);
            }
            if !schedule_children(backups, schedule).await?.is_empty() {
                return Ok(None);
            }
            for name in decoys {
                if backups.get_opt(name).await?.is_some() {
                    return Ok(None);
                }
            }
            Ok(Some(()))
        },
    )
    .await
}

/// `concurrencyPolicy: Replace` end to end, on an UNCAPPED repository.
///
/// The uncapped part is load-bearing, not incidental: with a pool cap in play a
/// due slot whose would-be victim is PARKED behind that cap does not replace
/// anything at all — it raises `ReplacementHeld` and waits, because cancelling a
/// queued run frees no capacity. This scenario must exercise the REPLACEMENT, so
/// the repository carries no cap and the victim is genuinely `Running` with a
/// live mover Job before the next slot arrives (asserted, not assumed).
///
/// Five claims:
///
/// * **The prior run is cancelled.** Its mover Job is deleted and its CR goes
///   away entirely — not merely terminating. Full disappearance is the sharp
///   form: a HELD deletion would sit terminating forever.
/// * **A `ReplacedActiveRun` Normal Event names the victim**, so an operator
///   reading `kubectl describe` sees why a backup vanished mid-run instead of
///   discovering a gap.
/// * **The replacement really backs something up** — `Succeeded` with a real
///   `kopiaSnapshotID`. A `Replace` that only cancelled would pass every
///   liveness check and be a data-loss bug.
/// * **The victim is EXEMPT from a breaker that is demonstrably firing** — the
///   `pruned-by: replaced-run` stamping proof. Read the mechanism carefully,
///   because the obvious version of this assertion is vacuous:
///
///   `pending_members` (`controller::snapshot::batch`) only counts a terminating
///   `Snapshot` that owns a `status.snapshot.kopiaSnapshotID`. A `Replace` victim
///   is unfinished BY CONSTRUCTION — its mover was still running, so it committed
///   no manifest — and therefore can never contribute to the breaker's pending
///   count no matter how it is classified. Arming a low threshold and asserting
///   "nothing was held" would pass just as happily with the stamp stripped out.
///
///   So the breaker is tripped by something that CAN count: [`DECOYS`], three
///   previously-`Succeeded` Snapshots (real manifests, `deletionPolicy: Delete`)
///   on this same repository, bulk-deleted into a HELD wave at
///   `deletionProtection.threshold: 1`. The scenario then asserts BOTH halves at
///   once — the decoy wave stays HELD (`DeletionHeld=True`/`MassDeletionBreaker`,
///   repo `MassDeletionHeld=True`) for the whole replacement window, while the
///   `Replace` victim sails straight through it and disappears entirely, never
///   carrying `DeletionHeld`.
///
///   That makes stripping the stamp a FAILING mutation. Unstamped, the victim's
///   `counts_toward_breaker` is `true` (`breaker_relevant(None)`, and its plan at
///   `BreakerState::Allowed` is `DeleteSnapshot`: `Origin::Scheduled` with no
///   `spec.deletionPolicy` resolves to `Delete`), so `breaker_applies` holds and
///   the repo-wide state the decoys already pushed to `Held` routes it to
///   `plan_external(Delete, Held)` → `HoldSnapshotDeletion`. That executor keeps
///   the finalizer and writes `DeletionHeld=True` BEFORE any manifest check (the
///   "no recorded kopia snapshot → just release" short-circuit lives inside the
///   `DeleteSnapshot` executor, which a Hold never reaches) — so the victim would
///   sit terminating forever and two assertions here would fail.
/// * **The schedule's status advances** — `lastSchedule` names the replacement
///   slot and its child.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn replace_cancels_the_running_backup_and_its_victim_is_breaker_exempt() -> anyhow::Result<()>
{
    let Some(world) = World::connect().await else {
        return Ok(());
    };
    world.ensure(&[Need::Filesystem]).await?;
    let client: Client = world.client().clone();

    const REPO: &str = "e2e-conc-repl-repo";
    const POLICY: &str = "e2e-conc-repl-pol";
    const DECOY_POLICY: &str = "e2e-conc-repl-dec-pol";
    const SCHEDULE: &str = "e2e-conc-repl-sched";

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let events: Api<Event> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // No `spec.concurrency` at all (see the doc comment), managed maintenance
    // off so the repository contributes no unrelated mover Jobs, and the breaker
    // DISARMED for now so the retry reset below can delete leftovers. The fast
    // catalog refresh is the same knob `mass_deletion`'s breaker scenarios use:
    // the repository's `MassDeletionHeld` condition is written LAZILY on the
    // repo's own reconcile cadence, so without a prompt re-reconcile the arming
    // handshake below would sit waiting on a condition that is merely late.
    ensure_repo(&client, REPLACE_SUBPATH).await;
    create_idempotent(
        &repos,
        &cr(repository_json(
            REPO,
            REPLACE_SUBPATH,
            serde_json::json!({
                "deletionProtection": { "threshold": 0 },
                "maintenance": { "enabled": false },
                "catalog": { "periodicRefresh": true, "refreshInterval": "30s" },
            }),
        )),
        "create Repository",
    )
    .await;
    set_deletion_threshold(&repos, REPO, 0).await?;
    wait_phase(&repos, REPO, "Ready").await.expect("repo Ready");
    ensure_policy(&client, POLICY, REPO).await;
    // The decoys get their OWN policy with a roomy `keepLatest`, mirroring
    // `mass_deletion`'s manual-vs-scheduled split: retention is label-scoped, so
    // this keeps the schedule's GFS prune (which is breaker-EXEMPT and would
    // quietly drain the wave) away from the decoy set entirely.
    let decoy_policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &decoy_policies,
        &cr(snapshot_policy_json(
            E2E_NAMESPACE,
            DECOY_POLICY,
            "Repository",
            REPO,
            serde_json::json!({
                "identity": { "username": DECOY_POLICY, "hostname": "e2e" },
                "retention": { "keepLatest": 20 },
            }),
        )),
        "create decoy SnapshotPolicy",
    )
    .await;
    reset_replace_fixtures(&schedules, &backups, SCHEDULE, &DECOYS).await?;

    // Seed the decoys at FULL SPEED (outside the fixture window) so each owns a
    // real kopia manifest — the property `pending_members` requires before a
    // terminating Snapshot counts toward the breaker at all.
    for name in DECOYS {
        create_idempotent(
            &backups,
            &cr(snapshot_json(
                E2E_NAMESPACE,
                name,
                DECOY_POLICY,
                serde_json::json!({ "deletionPolicy": "Delete" }),
            )),
            "create decoy Snapshot",
        )
        .await;
    }
    for name in DECOYS {
        assert_real_snapshot(&backups, name).await?;
    }

    // ARM the breaker at its most sensitive setting, now that nothing is left to
    // clean up. The patch bumps the repository's generation, so wait out the
    // reconcile it triggers before any backup depends on the repository.
    set_deletion_threshold(&repos, REPO, 1).await?;
    wait_phase(&repos, REPO, "Ready")
        .await
        .expect("repo Ready after arming the breaker");

    // TRIP the breaker: bulk-delete the decoys into a wave that must be HELD.
    // Three of them against `threshold: 1` for the reason `mass_deletion`'s
    // scenarios use a margin — a delete whose reconcile beats the reflector's
    // view of its own deletionTimestamp could momentarily count zero pending and
    // slip through; the survivors still satisfy `1 >= 1`.
    for name in DECOYS {
        backups.delete(name, &DeleteParams::default()).await?;
    }
    // Settle the deletionTimestamps before reading the breaker, the same guard
    // `mass_deletion::delete_snapshots_and_settle` applies: a loaded box must not
    // let a delete slip past a threshold check before the API even shows it.
    wait_until(
        "every decoy shows a deletionTimestamp",
        Duration::from_secs(120),
        INVARIANT_POLL,
        || async {
            for name in DECOYS {
                match backups.get_opt(name).await? {
                    // Already drained (it beat the breaker) counts as settled.
                    None => continue,
                    Some(s) if s.metadata.deletion_timestamp.is_some() => continue,
                    Some(_) => return Ok(None),
                }
            }
            Ok(Some(()))
        },
    )
    .await?;
    wait_condition(&repos, REPO, MASS_DELETION_HELD_CONDITION, "True")
        .await
        .expect(
            "the decoy wave must trip the repository breaker — without a FIRING breaker the \
             victim's exemption below would prove nothing",
        );
    let held_decoy = wait_until(
        "a decoy Snapshot is HELD by the mass-deletion breaker",
        Duration::from_secs(300),
        INVARIANT_POLL,
        || async {
            for name in DECOYS {
                if condition_status(&backups, name, DELETION_HELD_CONDITION)
                    .await
                    .as_deref()
                    == Some("True")
                {
                    return Ok(Some(name));
                }
            }
            Ok(None)
        },
    )
    .await?;
    let held = wait_condition(&backups, held_decoy, DELETION_HELD_CONDITION, "True").await?;
    anyhow::ensure!(
        held["reason"].as_str() == Some(MASS_DELETION_BREAKER_REASON),
        "the decoy must be held by the BREAKER specifically, got {held}"
    );

    // Only backups crawl: the bootstrap already ran at full speed above, and a
    // slow repository-side Job would add nothing but wall time.
    let config = SlowMover::new(REPLACE_DELAY).ops(&[MoverOp::Snapshot]);
    let result = with_slow_mover_config(&world, config, || async {
        schedules
            .create(
                &PostParams::default(),
                &cr::<SnapshotSchedule>(serde_json::json!({
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "SnapshotSchedule",
                    "metadata": { "name": SCHEDULE, "namespace": E2E_NAMESPACE },
                    "spec": {
                        "policyRef": { "name": POLICY },
                        "schedule": { "cron": "* * * * *", "concurrencyPolicy": "Replace" },
                    }
                })),
            )
            .await?;
        // The field must have LANDED: a `concurrencyPolicy` pruned by a stale CRD
        // schema would silently leave the schedule at the `Forbid` default, where
        // nothing is ever replaced and this scenario would fail minutes later as
        // "no replacement happened".
        let created = schedules.get(SCHEDULE).await?;
        anyhow::ensure!(
            created.spec.schedule.concurrency_policy == kopiur_api::ConcurrencyPolicy::Replace,
            "spec.schedule.concurrencyPolicy must land as Replace, got {:?}",
            created.spec.schedule.concurrency_policy
        );

        // 1. A slot fires and its child reaches `Running` with a LIVE mover Job.
        //    This is what makes the replacement about a genuinely in-flight run
        //    rather than a `Pending` one that never started.
        let victim = wait_until(
            "a scheduled backup is Running with a live mover Job",
            Duration::from_secs(300),
            INVARIANT_POLL,
            || async {
                for name in schedule_children(&backups, SCHEDULE).await? {
                    let running =
                        status_json(&backups, &name).await["phase"].as_str() == Some("Running");
                    if running && live_jobs(&jobs, &[name.as_str()]).await? == 1 {
                        return Ok(Some(name));
                    }
                }
                Ok(None)
            },
        )
        .await?;

        // 2. The NEXT slot must replace it. `REPLACE_DELAY` makes that a
        //    certainty rather than a race: the victim cannot reach a terminal
        //    phase before the slot 60s from now. Polled with the breaker and
        //    hold invariants inline — `wait_until` treats an `Err` as "not ready
        //    yet", so an assertion cannot live inside its closure.
        //
        //    The never-`True` invariants read conditions through
        //    `condition_status`, which reports an API error as "condition
        //    absent" — best-effort polling, in keeping with `wait_until`'s own
        //    blip tolerance. A missed poll can only WEAKEN such an assertion,
        //    never invent a violation, and the states involved persist for many
        //    polls. The must-be-`True` breaker check is the other polarity, so it
        //    goes through `condition_status_checked` and skips the poll on an
        //    API error rather than reading the blip as the breaker relaxing.
        let mut replacement = None;
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            // The breaker must still be FIRING throughout, or the victim's
            // clean exit below proves nothing about its exemption.
            if let Ok(held) =
                condition_status_checked(&repos, REPO, MASS_DELETION_HELD_CONDITION).await
            {
                anyhow::ensure!(
                    held.as_deref() == Some("True"),
                    "the decoy wave must stay HELD for the whole replacement window — a \
                     breaker that fell back below threshold would make the victim's \
                     exemption vacuous"
                );
            }
            anyhow::ensure!(
                condition_status(&backups, &victim, DELETION_HELD_CONDITION)
                    .await
                    .as_deref()
                    != Some("True"),
                "the `Replace` victim must be EXEMPT from the (currently firing) \
                 mass-deletion breaker: it is stamped `pruned-by: replaced-run`, an \
                 OPERATOR prune, and operator prunes are never held"
            );
            anyhow::ensure!(
                condition_status(&schedules, SCHEDULE, REPLACEMENT_HELD_CONDITION)
                    .await
                    .as_deref()
                    != Some("True"),
                "{REPLACEMENT_HELD_CONDITION} must be unreachable on an UNCAPPED \
                 repository — if it fired, the victim was parked rather than Running \
                 and this scenario tested the hold path, not the replacement"
            );
            let victim_going = match backups.get_opt(&victim).await? {
                None => true,
                Some(s) => s.metadata.deletion_timestamp.is_some(),
            };
            if victim_going {
                // The newest child NEWER THAN the victim. Children are named
                // `<schedule>-<YYYYmmddHHMMSS>` — fixed width, so lexicographic
                // order IS chronological order. Strictly-newer rather than
                // merely not-equal: a child OLDER than the victim can only be
                // one wedged from an earlier slot, and picking it would fail
                // this scenario for something it is not testing. `max` rather
                // than `find` because a poll that lagged a whole slot interval
                // would otherwise be free to pick an already-superseded child
                // and then assert "Succeeded" against a run that was itself
                // cancelled.
                replacement = schedule_children(&backups, SCHEDULE)
                    .await?
                    .into_iter()
                    .filter(|n| n.as_str() > victim.as_str())
                    .max();
            }
            if replacement.is_some() {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "the next slot never replaced the Running backup `{victim}` \
                 (victim still in flight: {})",
                !victim_going
            );
            tokio::time::sleep(INVARIANT_POLL).await;
        }
        let replacement = replacement.expect("the loop only breaks with a replacement");

        // 3. SUSPEND immediately, so the slot after this one cannot replace the
        //    replacement in turn — every run takes `REPLACE_DELAY`, so an
        //    unsuspended schedule would cancel each new run forever and nothing
        //    would ever reach `Succeeded`. There is a full slot interval of
        //    headroom: the replacement was detected within a poll of the fire.
        schedules
            .patch(
                SCHEDULE,
                &PatchParams::default(),
                &Patch::Merge(serde_json::json!({
                    "spec": { "schedule": { "suspend": true } }
                })),
            )
            .await?;

        // 4. The victim's mover Job is gone — the run was STOPPED, not merely
        //    forgotten. (The Job carries the same name as its Snapshot.)
        wait_until(
            &format!("the replaced run's mover Job ({victim}) is deleted"),
            Duration::from_secs(180),
            INVARIANT_POLL,
            || async { Ok(jobs.get_opt(&victim).await?.is_none().then_some(())) },
        )
        .await?;

        // 5. The victim's CR disappears ENTIRELY, WHILE the decoy wave is still
        //    held on this very repository. That pairing is the whole exemption
        //    proof: unstamped, the victim would take `plan_external(Delete,
        //    Held)` → `HoldSnapshotDeletion`, keep its finalizer, and sit here
        //    terminating with `DeletionHeld=True` forever.
        let mut gone = false;
        let deadline = Instant::now() + Duration::from_secs(180);
        while !gone {
            if let Ok(held) =
                condition_status_checked(&repos, REPO, MASS_DELETION_HELD_CONDITION).await
            {
                anyhow::ensure!(
                    held.as_deref() == Some("True"),
                    "the decoy wave must still be HELD while the victim drains — otherwise \
                     the victim merely outlived the breaker instead of being exempt from it"
                );
            }
            match backups.get_opt(&victim).await? {
                None => gone = true,
                Some(_) => {
                    anyhow::ensure!(
                        condition_status(&backups, &victim, DELETION_HELD_CONDITION)
                            .await
                            .as_deref()
                            != Some("True"),
                        "the replaced run's deletion must never be HELD: `Replace` stamps \
                         `pruned-by: replaced-run`, which is breaker-exempt"
                    );
                }
            }
            anyhow::ensure!(
                gone || Instant::now() < deadline,
                "the replaced run `{victim}` never finished terminating — its finalizer \
                 is stuck (a held deletion, or a mover teardown that never completed)"
            );
            if !gone {
                tokio::time::sleep(INVARIANT_POLL).await;
            }
        }

        // 6. The cancellation is VISIBLE: an Event on the schedule NAMING the run
        //    it cancelled, so an operator reading `kubectl describe` learns why a
        //    backup vanished mid-run instead of finding an unexplained gap.
        //
        //    The victim's name is part of the SELECTION, not just an assertion
        //    afterwards: the `e2e` profile retries in place, victim names are
        //    slot-stamped (fresh every attempt), and a stale Event from a
        //    previous attempt would otherwise be picked up and then fail the
        //    note check — turning a real pass into a spurious failure.
        let ev = wait_until(
            &format!("a {REPLACED_EVENT_REASON} Event names the cancelled run {victim}"),
            Duration::from_secs(180),
            INVARIANT_POLL,
            || async {
                find_event(&events, |e| {
                    e.reason.as_deref() == Some(REPLACED_EVENT_REASON)
                        && e.note.as_deref().is_some_and(|n| n.contains(&victim))
                        && e.regarding.as_ref().is_some_and(|r| {
                            r.kind.as_deref() == Some("SnapshotSchedule")
                                && r.name.as_deref() == Some(SCHEDULE)
                        })
                })
                .await
            },
        )
        .await?;
        anyhow::ensure!(
            ev.type_.as_deref() == Some("Normal"),
            "a planned cancellation is Normal, not a Warning: {:?}",
            ev.type_
        );

        Ok(replacement)
    })
    .await;

    // UNCONDITIONAL, before ANY `?` on the body's result — the
    // `set_global_job_cap` restore discipline, applied to this scenario's own two
    // cluster-visible mutations. A body failure before step 3 would otherwise
    // leave BOTH of them live: an unsuspended `* * * * *` `Replace` schedule that
    // keeps minting slow mover Jobs (which
    // `the_env_backstop_serializes_across_repositories` then has to share its
    // cluster-wide pool of 1 with, so that scenario fails for a reason that has
    // nothing to do with it), and an armed threshold-1 breaker that holds every
    // later external delete on this repository. Suspend rather than delete: the
    // assertions below still need the schedule's status.
    let suspended = schedules
        .patch(
            SCHEDULE,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "schedule": { "suspend": true } } })),
        )
        .await;
    let disarmed = set_deletion_threshold(&repos, REPO, 0).await;

    // Asserted OUTSIDE the slow window, like the other scenarios here, so a
    // fixture-restore failure can never be mistaken for a backup fault. The
    // schedule is suspended by now, so nothing new fires while we wait.
    let replacement = result?;
    suspended?;
    disarmed?;
    assert_real_snapshot(&backups, &replacement).await?;

    // The schedule's own bookkeeping advanced to the slot that did the
    // replacing. `lastSchedule.snapshotRef` names the child, and `at` is that
    // child's slot — the Snapshot's name is `<schedule>-<YYYYmmddHHMMSS>` of the
    // slot, so the two must agree digit for digit.
    let slot_stamp = replacement
        .strip_prefix(&format!("{SCHEDULE}-"))
        .expect("a scheduled child is named <schedule>-<slot stamp>")
        .to_string();
    let last = wait_until(
        "status.lastSchedule names the replacement slot",
        Duration::from_secs(180),
        INVARIANT_POLL,
        || async {
            let s = status_json(&schedules, SCHEDULE).await;
            let named = s["lastSchedule"]["snapshotRef"]["name"].as_str() == Some(&replacement);
            Ok(named.then(|| s["lastSchedule"].clone()))
        },
    )
    .await?;
    let at = last["at"].as_str().unwrap_or_default();
    let at = chrono::DateTime::parse_from_rfc3339(at)
        .unwrap_or_else(|e| panic!("lastSchedule.at must be RFC3339, got {at:?}: {e}"))
        .with_timezone(&chrono::Utc);
    anyhow::ensure!(
        at.format("%Y%m%d%H%M%S").to_string() == slot_stamp,
        "lastSchedule.at ({at}) must be the slot the replacement child is named for \
         ({slot_stamp})"
    );
    // `lastSuccessfulSchedule` has no writer in this build; assert only that if
    // one ever appears it is not AHEAD of the last fire (a lenient sanity bound
    // that a future writer would still satisfy).
    let st = status_json(&schedules, SCHEDULE).await;
    if let Some(s) = st["lastSuccessfulSchedule"]["at"].as_str() {
        let s = chrono::DateTime::parse_from_rfc3339(s)
            .unwrap_or_else(|e| panic!("lastSuccessfulSchedule.at must be RFC3339: {e}"))
            .with_timezone(&chrono::Utc);
        anyhow::ensure!(
            s <= at,
            "lastSuccessfulSchedule ({s}) cannot be later than lastSchedule ({at})"
        );
    }

    // RELEASE the wave. The breaker was already disarmed unconditionally above,
    // which drops `unacked_pending >= threshold` and lets the held decoys drain
    // on their own (`repo_mass_deletion_condition` is `held: false` whenever the
    // threshold is `0`) — no `allow-mass-deletion` ack needed. Wait for it, so
    // cleanup does not race a still-terminating wave.
    let _ = wait_until(
        "the released decoy wave drains once the breaker is disarmed",
        Duration::from_secs(300),
        INVARIANT_POLL,
        || async {
            for name in DECOYS {
                if backups.get_opt(name).await?.is_some() {
                    return Ok(None);
                }
            }
            Ok(Some(()))
        },
    )
    .await;

    let _ = schedules.delete(SCHEDULE, &DeleteParams::default()).await;
    let leftovers = schedule_children(&backups, SCHEDULE)
        .await
        .unwrap_or_default();
    let mut leftovers: Vec<&str> = leftovers.iter().map(String::as_str).collect();
    leftovers.extend(DECOYS);
    cleanup(&client, &[REPO], &[POLICY, DECOY_POLICY], &leftovers).await;
    Ok(())
}
