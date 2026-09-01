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
//! The four scenarios:
//!
//! 1. `a_repository_cap_serializes_its_backups` — cap 1, two backups: never two
//!    live at once, the loser surfaces `RepositorySlotAvailable=False`, BOTH
//!    still succeed with a real kopia snapshot, and the loser's condition heals
//!    to `True` once it runs (the heal-ordering regression).
//! 2. `a_restore_is_never_queued_behind_backups` — cap 1 with a backup holding
//!    the slot: a Restore gets its Job immediately while a further backup parks,
//!    and the restore completes.
//! 3. `an_uncapped_repository_never_grows_the_slot_condition` — the default
//!    install must be indistinguishable from a build without the feature.
//! 4. `the_env_backstop_serializes_across_repositories` —
//!    `KOPIUR_MAX_CONCURRENT_JOBS=1` bounds the pool across DIFFERENT
//!    repositories, which no per-repository cap can do.

#![cfg(all(unix, feature = "e2e"))]

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};

use kopiur_api::{Repository, Restore, Snapshot, SnapshotPolicy};
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

/// The `status` of the CR's `RepositorySlotAvailable` condition, or `None` when
/// the condition is absent entirely — the distinction the uncapped scenario
/// turns on (absent, never merely `True`).
async fn slot_condition_status<K>(api: &Api<K>, name: &str) -> Option<String>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + serde::Serialize + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    status_json(api, name)
        .await
        .get("conditions")
        .and_then(|c| c.as_array())
        .and_then(|a| {
            a.iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some(SLOT_CONDITION))
        })
        .and_then(|c| c.get("status").and_then(|s| s.as_str()))
        .map(str::to_string)
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
