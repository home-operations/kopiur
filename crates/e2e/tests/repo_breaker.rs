//! e2e: the repository circuit breaker (#345).
//!
//! The incident this guards: a backend outage (Garage down for hours) while the
//! probe-less operator kept launching scheduled backups produced **53 Failed
//! Snapshot CRs and 23 dead mover Jobs** against one repository. The breaker
//! closes that hole: past `spec.health.probe.failureThreshold` consecutive
//! failed connects (default `onFailure: Degrade`) the repository moves to
//! `Degraded` (`BackendReachable=False`, `Ready=False`), every consumer gate
//! closes (scheduled Snapshots park `Pending`, bounded at one by
//! `concurrencyPolicy: Forbid`), and recovery is automatic: the strict retry
//! loop re-connects on a 120s→600s backoff and ANY successful connect heals the
//! repository back to `Ready`, at which point the pinned stale slot fires its
//! one catch-up backup.
//!
//! ## Outage mechanism — Service-selector break, NOT MinIO scale-down
//!
//! MinIO's `/data` is an `emptyDir`: scaling the Deployment to 0 would WIPE the
//! repository, so recovery could never pass (and a wipe is the `Vanished`
//! escalation path, terminal by design — not the outage arc this file tests).
//! Instead the `minio` Service's selector is patched to a non-matching label
//! (`app: minio-offline`) → endpoints empty → `kopia repository connect` gets
//! connection refused → classifies `RepositoryUnavailable`. Patching the
//! selector back to `app: minio` restores the backend with all data intact.
//!
//! Breaking the selector takes down MinIO for the whole cluster, so this binary
//! OWNS its CI shard (see the shard-ownership convention in
//! `crates/e2e/src/lib.rs`) and both repositories (Degrade-mode + the
//! Alert-mode opt-out) ride the SAME outage window inside ONE test function —
//! two `#[tokio::test]`s would race each other's break/heal.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Service;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client, ResourceExt};

use common::cr;
use kopiur_api::{Repository, Snapshot, SnapshotPolicy, SnapshotSchedule};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, consts, default_timeout, poll_interval, scrape_controller_metrics,
    wait_until,
};

const REPO: &str = "e2e-breaker-repo";
const ALERT_REPO: &str = "e2e-breaker-alert";
const POLICY: &str = "e2e-breaker-pol";
const SCHEDULE: &str = "e2e-breaker-sched";

/// The label a `SnapshotSchedule` stamps on every Snapshot it produces (and the
/// backup mover Jobs inherit via the Snapshot's `run_labels`). A wire-contract
/// literal, same as `common::WORK_SPEC_ENV`: a rename in the controller must
/// fail this suite.
const SCHEDULE_LABEL: &str = "kopiur.home-operations.com/schedule";

/// How long past the breaker OPEN the scenario keeps observing that no new
/// backup work is attempted: ~2.5 every-minute schedule slots.
const OPEN_HOLD: Duration = Duration::from_secs(150);

/// Budget for the repository healing back to `Ready` after the selector is
/// restored. Deliberately LONGER than `default_timeout()`: recovery rides the
/// strict retry loop, whose launch-side holdoff backs off exponentially from
/// 120s to a 600s cap per consecutive failure (`health::strict_retry_backoff`),
/// so a heal landing just after a failed retry can legitimately wait the full
/// 600s before the next (successful) connect even launches — plus Job
/// schedule/run time and a watch-reconnect margin.
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(900);

/// An S3 `Repository` on `bucket` with a fast probe cadence (`interval: 30s`,
/// `failureThreshold: 2`) so the breaker trips within the e2e budget. The
/// Degrade-mode repo leaves `onFailure` absent to prove the DEFAULT is the
/// breaker; the Alert-mode repo passes `Some("Alert")` for the opt-out.
fn breaker_repository_json(
    name: &str,
    bucket: &str,
    on_failure: Option<&str>,
) -> serde_json::Value {
    let mut probe = serde_json::json!({
        "enabled": true,
        "interval": "30s",
        "failureThreshold": 2,
    });
    if let Some(mode) = on_failure {
        probe["onFailure"] = serde_json::json!(mode);
    }
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "backend": { "s3": {
                "bucket": bucket,
                "endpoint": consts::MINIO_ENDPOINT,
                "region": "us-east-1",
                "tls": { "disableTls": true },
                "auth": { "secretRef": { "name": consts::SECRET_S3_CREDS, "namespace": E2E_NAMESPACE } }
            }},
            "encryption": {
                "passwordSecretRef": { "name": consts::SECRET_S3_CREDS, "key": "KOPIA_PASSWORD" }
            },
            "create": { "enabled": true },
            // The managed Maintenance would only add unrelated Jobs to count.
            "maintenance": { "enabled": false },
            "health": { "probe": probe }
        }
    })
}

fn status_value(repo: &Repository) -> serde_json::Value {
    serde_json::to_value(repo)
        .ok()
        .and_then(|v| v.get("status").cloned())
        .unwrap_or_default()
}

fn condition(status: &serde_json::Value, type_: &str, field: &str) -> Option<String> {
    status
        .get("conditions")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some(type_))
        .and_then(|c| c.get(field).and_then(|s| s.as_str()))
        .map(str::to_string)
}

/// Point the `minio` Service's selector at `app` via a merge patch. `"minio"`
/// matches the Deployment's pod label (endpoints populated); anything else
/// empties the endpoints — the outage.
async fn set_minio_selector(client: &Client, app: &str) -> anyhow::Result<()> {
    let services: Api<Service> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    services
        .patch(
            "minio",
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({ "spec": { "selector": { "app": app } } })),
        )
        .await?;
    eprintln!("[repo_breaker] minio Service selector → app={app}");
    Ok(())
}

/// Phase population of the schedule's produced Snapshots (listed by the
/// schedule label, so manual/discovered rows never skew the counts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PhaseCounts {
    succeeded: usize,
    failed: usize,
    /// Anything not yet terminal (Pending/Running/…): the Forbid bound applies here.
    nonterminal: usize,
}

/// Returns `kube::Error` (not `anyhow`) so it composes directly into
/// `wait_until` closures.
async fn phase_counts(backups: &Api<Snapshot>) -> Result<PhaseCounts, kube::Error> {
    let lp = ListParams::default().labels(&format!("{SCHEDULE_LABEL}={SCHEDULE}"));
    let list = backups.list(&lp).await?;
    let mut counts = PhaseCounts {
        succeeded: 0,
        failed: 0,
        nonterminal: 0,
    };
    for b in &list.items {
        let phase = serde_json::to_value(b)
            .ok()
            .and_then(|v| {
                v.pointer("/status/phase")
                    .and_then(|p| p.as_str().map(str::to_string))
            })
            .unwrap_or_default();
        match phase.as_str() {
            "Succeeded" => counts.succeeded += 1,
            "Failed" => counts.failed += 1,
            _ => counts.nonterminal += 1,
        }
    }
    Ok(counts)
}

/// Distinct UIDs of mover Jobs carrying the schedule label (backup runs for
/// this scenario's Snapshots). Finished Jobs may be TTL-reaped, so callers
/// accumulate UIDs across polls — a NEW uid during the open window means the
/// gate leaked a launch.
async fn schedule_job_uids(jobs: &Api<Job>) -> Result<BTreeSet<String>, kube::Error> {
    let lp = ListParams::default().labels(&format!("{SCHEDULE_LABEL}={SCHEDULE}"));
    let list = jobs.list(&lp).await?;
    Ok(list.items.iter().filter_map(|j| j.uid()).collect())
}

/// Sum the values of every `metric{...}` sample whose label set contains all of
/// `labels`. Test-local Prometheus-text parsing, same pattern as
/// `steady_state.rs::reconciliations_by_kind`.
fn metric_sum(text: &str, metric: &str, labels: &[(&str, &str)]) -> Option<f64> {
    let mut sum = None;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(&format!("{metric}{{")) else {
            continue;
        };
        let Some((label_str, value)) = rest.rsplit_once("} ") else {
            continue;
        };
        if !labels
            .iter()
            .all(|(k, v)| label_str.contains(&format!("{k}=\"{v}\"")))
        {
            continue;
        }
        if let Ok(v) = value.trim().parse::<f64>() {
            *sum.get_or_insert(0.0) += v;
        }
    }
    sum
}

/// The full breaker arc: baseline → outage (selector break) → breaker OPEN
/// (phase `Degraded`, consumers parked, no failure pile-up) → heal → automatic
/// recovery (`Ready`) → the parked slot's single catch-up backup → metrics.
/// The Alert-mode repository rides the same outage and must stay `Ready`
/// throughout (the opt-out contract).
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn breaker_pauses_backups_during_outage_and_recovers() {
    let Some(world) = World::connect().await else {
        return;
    };
    // Filesystem for the shared `e2e-src` source PVC; Minio for the buckets.
    world
        .ensure(&[Need::Filesystem, Need::Minio])
        .await
        .expect("provision filesystem + MinIO fixtures");
    let client = world.client().clone();

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // --- 0. Re-entrancy: nextest's e2e profile RETRIES a failed test in the
    // same cluster, and a mid-arc panic can leave the selector broken and the
    // CRs behind — a retry must start from a clean slate, not fail fast on
    // AlreadyExists against half-dead state. Heal the selector first (Snapshot
    // finalizers need a reachable backend to drain), then drain any prior
    // attempt's CRs. All best-effort + wait-gone; a fresh cluster no-ops.
    set_minio_selector(&client, "minio")
        .await
        .expect("reset the minio Service selector");
    let _ = schedules.delete(SCHEDULE, &Default::default()).await;
    let _ = policies.delete(POLICY, &Default::default()).await;
    if let Ok(list) = backups.list(&Default::default()).await {
        for b in list.items {
            if let Some(n) = b.metadata.name.as_deref()
                && n.starts_with(SCHEDULE)
            {
                let _ = backups.delete(n, &Default::default()).await;
            }
        }
    }
    let _ = repos.delete(REPO, &Default::default()).await;
    let _ = repos.delete(ALERT_REPO, &Default::default()).await;
    wait_until(
        "prior-attempt CRs fully drained",
        default_timeout(),
        poll_interval(),
        || async {
            let gone = repos.get_opt(REPO).await?.is_none()
                && repos.get_opt(ALERT_REPO).await?.is_none()
                && policies.get_opt(POLICY).await?.is_none()
                && schedules.get_opt(SCHEDULE).await?.is_none();
            Ok(gone.then_some(()))
        },
    )
    .await
    .expect("prior-attempt CRs should drain before the fresh run");

    // --- 1. Baseline: repos Ready, one scheduled backup Succeeded. ------------
    repos
        .create(
            &PostParams::default(),
            &cr(breaker_repository_json(
                REPO,
                consts::BUCKET_BREAKER_REPO,
                None, // absent onFailure — the DEFAULT must be the breaker
            )),
        )
        .await
        .expect("create breaker Repository");
    repos
        .create(
            &PostParams::default(),
            &cr(breaker_repository_json(
                ALERT_REPO,
                consts::BUCKET_BREAKER_ALERT,
                Some("Alert"),
            )),
        )
        .await
        .expect("create Alert-mode Repository");
    policies
        .create(
            &PostParams::default(),
            &cr(common::snapshot_policy_json(
                E2E_NAMESPACE,
                POLICY,
                "Repository",
                REPO,
                serde_json::json!({}),
            )),
        )
        .await
        .expect("create SnapshotPolicy");
    common::wait_phase(&repos, REPO, "Ready")
        .await
        .expect("breaker Repository should reach Ready");
    common::wait_phase(&repos, ALERT_REPO, "Ready")
        .await
        .expect("Alert-mode Repository should reach Ready");

    // Every-minute cron + runOnCreate for a fast baseline; concurrencyPolicy
    // defaults to Forbid — the bound the parked-Pending assertion rides on.
    schedules
        .create(
            &PostParams::default(),
            &cr(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "SnapshotSchedule",
                "metadata": { "name": SCHEDULE, "namespace": E2E_NAMESPACE },
                "spec": {
                    "policyRef": { "name": POLICY },
                    "schedule": { "cron": "* * * * *", "runOnCreate": true }
                }
            })),
        )
        .await
        .expect("create SnapshotSchedule");

    wait_until(
        "a scheduled Snapshot Succeeds (baseline)",
        default_timeout(),
        poll_interval(),
        || async { Ok((phase_counts(&backups).await?.succeeded >= 1).then_some(())) },
    )
    .await
    .expect("baseline scheduled backup should Succeed");
    let baseline = phase_counts(&backups).await.expect("baseline counts");
    let mut seen_job_uids = schedule_job_uids(&jobs).await.expect("baseline Job uids");
    eprintln!(
        "[repo_breaker] baseline: {baseline:?}, {} Job uid(s)",
        seen_job_uids.len()
    );

    // --- 2..4: the outage window. Runs as a fallible block (anyhow::ensure!,
    // no panics) so the selector is ALWAYS restored afterwards — a failed
    // assertion must not leave MinIO broken for a same-run rerun.
    set_minio_selector(&client, "minio-offline")
        .await
        .expect("break the minio Service selector");

    let outage = async {
        // 2. The breaker must OPEN: BackendReachable=False, then phase Degraded
        //    with Ready=False. Condition-gated first, phase second (two-pass
        //    heal discipline). While the breaker trips, the Failed population
        //    may grow by AT MOST `failureThreshold` (2 here): the debounce is
        //    real — every sensor tick below the threshold keeps the repository
        //    Ready, and one Forbid-bounded backup can legitimately fail inside
        //    each ~30-60s tick window. That bounded cost IS the design ("a
        //    few, not 53"); what must NEVER happen is one failure per schedule
        //    tick for the whole outage (the 53-Failed-CRs incident).
        let allowed_failed_during_trip = baseline.failed + 2;
        wait_until(
            &format!("{REPO} BackendReachable=False"),
            default_timeout(),
            poll_interval(),
            || async {
                let s = status_value(&repos.get(REPO).await?);
                Ok(
                    (condition(&s, "BackendReachable", "status").as_deref() == Some("False"))
                        .then_some(()),
                )
            },
        )
        .await?;
        // Resolves as soon as EITHER the breaker opens (Ok) or the Failed
        // population blows the bound (Err) — the pile-up must not have to wait
        // for a timeout to be diagnosed, but it also must not panic past the
        // selector restore below.
        let trip: Result<(), String> = wait_until(
            &format!("{REPO} phase=Degraded with Ready=False (breaker OPEN)"),
            default_timeout(),
            poll_interval(),
            || async {
                let counts = phase_counts(&backups).await?;
                if counts.failed > allowed_failed_during_trip {
                    return Ok(Some(Err(format!(
                        "Failed Snapshots piled up while the breaker was tripping: \
                         {} > allowed {} (baseline {} + the failureThreshold debounce window) — \
                         the #345 failure-per-tick regression",
                        counts.failed, allowed_failed_during_trip, baseline.failed
                    ))));
                }
                let s = status_value(&repos.get(REPO).await?);
                let open = s.get("phase").and_then(|p| p.as_str()) == Some("Degraded")
                    && condition(&s, "Ready", "status").as_deref() == Some("False");
                Ok(open.then_some(Ok(())))
            },
        )
        .await?;
        trip.map_err(|msg| anyhow::anyhow!(msg))?;
        let at_open = phase_counts(&backups).await?;
        anyhow::ensure!(
            at_open.failed <= allowed_failed_during_trip,
            "Failed count at breaker OPEN: {} > allowed {}",
            at_open.failed,
            allowed_failed_during_trip
        );
        seen_job_uids.extend(schedule_job_uids(&jobs).await?);
        eprintln!(
            "[repo_breaker] breaker OPEN: {at_open:?}, {} Job uid(s)",
            seen_job_uids.len()
        );

        // The Alert-mode repository sees the SAME outage but must stay Ready:
        // BackendReachable=False while the phase never leaves Ready (the
        // opt-out contract the old health_probe wipe test asserted globally).
        let alert: Result<(), String> = wait_until(
            &format!("{ALERT_REPO} BackendReachable=False while staying Ready"),
            default_timeout(),
            poll_interval(),
            || async {
                let s = status_value(&repos.get(ALERT_REPO).await?);
                let phase = s
                    .get("phase")
                    .and_then(|p| p.as_str())
                    .unwrap_or("")
                    .to_string();
                if phase != "Ready" {
                    return Ok(Some(Err(format!(
                        "onFailure: Alert must keep the repository Ready through an \
                         outage, got phase {phase:?} — the opt-out contract is broken"
                    ))));
                }
                Ok(
                    (condition(&s, "BackendReachable", "status").as_deref() == Some("False"))
                        .then_some(Ok(())),
                )
            },
        )
        .await?;
        alert.map_err(|msg| anyhow::anyhow!(msg))?;

        // 3. Hold the breaker open across ~2.5 schedule slots. Every poll:
        //    the phase stays Degraded (STABLE while open — no Initializing
        //    flap), parked Pending Snapshots stay ≤ 1 (the Forbid bound: one
        //    pinned catch-up, never one per slot), the Failed population does
        //    not grow, and no NEW backup mover Job is launched.
        let deadline = Instant::now() + OPEN_HOLD;
        while Instant::now() < deadline {
            let counts = phase_counts(&backups).await?;
            anyhow::ensure!(
                counts.nonterminal <= 1,
                "concurrencyPolicy Forbid must bound parked Pending Snapshots at 1 \
                 while the breaker is open, saw {} nonterminal ({counts:?})",
                counts.nonterminal
            );
            anyhow::ensure!(
                counts.failed == at_open.failed,
                "no Snapshot may FAIL while the breaker is open (the gate parks \
                 them Pending): failed went {} → {}",
                at_open.failed,
                counts.failed
            );
            let s = status_value(&repos.get(REPO).await?);
            let phase = s.get("phase").and_then(|p| p.as_str()).unwrap_or("");
            anyhow::ensure!(
                phase == "Degraded",
                "the phase must hold Degraded STABLY while the breaker is open \
                 (the M4 launch-phase guarantee), saw {phase:?}"
            );
            let now_uids = schedule_job_uids(&jobs).await?;
            let new: Vec<_> = now_uids.difference(&seen_job_uids).cloned().collect();
            anyhow::ensure!(
                new.is_empty(),
                "a NEW backup mover Job was launched while the breaker was open \
                 (the gate leaked): {new:?}"
            );
            tokio::time::sleep(poll_interval()).await;
        }
        let at_heal = phase_counts(&backups).await?;
        eprintln!("[repo_breaker] held open {OPEN_HOLD:?}: {at_heal:?}");
        Ok::<PhaseCounts, anyhow::Error>(at_heal)
    }
    .await;

    // 4. ALWAYS heal the selector, even when the outage block failed; only then
    //    propagate the block's result.
    let healed = set_minio_selector(&client, "minio").await;
    let at_heal = outage.expect("outage-window assertions");
    healed.expect("restore the minio Service selector");

    // Recovery: the strict retry loop re-connects (120s→600s backoff) and any
    // successful connect heals the repository — no operator action, no spec
    // change. See RECOVERY_TIMEOUT for the budget rationale.
    wait_until(
        &format!("{REPO} recovers to Ready"),
        RECOVERY_TIMEOUT,
        poll_interval(),
        || async {
            let s = status_value(&repos.get(REPO).await?);
            Ok((s.get("phase").and_then(|p| p.as_str()) == Some("Ready")).then_some(()))
        },
    )
    .await
    .expect("repository must heal to Ready automatically after the backend returns");
    // The whole outage produced at most ONE pinned catch-up CR (Forbid), so at
    // recovery the herd is provably absent: ≤ 1 nonterminal Snapshot exists.
    let at_ready = phase_counts(&backups).await.expect("counts at recovery");
    assert!(
        at_ready.nonterminal <= 1,
        "at recovery there must be at most the single pinned catch-up Snapshot, \
         saw {} nonterminal ({at_ready:?})",
        at_ready.nonterminal
    );
    assert_eq!(
        at_ready.failed, at_heal.failed,
        "no Snapshot may fail during the recovery wait (the gate stays closed \
         until Ready)"
    );

    // The parked slot fires exactly once: a NEW Succeeded Snapshot appears, and
    // the Failed population never grows. (After recovery the every-minute cron
    // resumes on its own, so "exactly one catch-up" is asserted as the FIRST
    // new success arriving with zero new failures — a later natural slot's
    // success is indistinguishable from a catch-up by design, and the herd
    // bound above already proved the outage minted only one pending CR.)
    wait_until(
        "the parked slot's catch-up backup Succeeds",
        default_timeout(),
        poll_interval(),
        || async {
            let counts = phase_counts(&backups).await?;
            assert_eq!(
                counts.failed, at_heal.failed,
                "recovery must not fail Snapshots ({counts:?})"
            );
            Ok((counts.succeeded > at_heal.succeeded).then_some(()))
        },
    )
    .await
    .expect("the pinned stale slot must fire its catch-up backup after recovery");

    // The Alert-mode repository is still Ready (it never left).
    let alert_status = status_value(&repos.get(ALERT_REPO).await.expect("get alert repo"));
    assert_eq!(
        alert_status.get("phase").and_then(|p| p.as_str()),
        Some("Ready"),
        "the Alert-mode repository must be Ready after the outage"
    );

    // 5. Metrics: the trip was counted, and no Snapshot is gated any more.
    //    Scraped inside a wait loop so a collection-cycle race can settle
    //    instead of flaking.
    wait_until(
        "breaker metrics reflect the trip and the drained gate",
        default_timeout(),
        poll_interval(),
        || async {
            let text = scrape_controller_metrics(&client)
                .await
                .map_err(|e| kube::Error::Service(e.into()))?;
            let trips = metric_sum(
                &text,
                "kopiur_repository_breaker_trips_total",
                &[("kind", "Repository"), ("name", REPO)],
            )
            .unwrap_or(0.0);
            let gated = metric_sum(&text, "kopiur_snapshot_gated", &[("policy", POLICY)]);
            let ok = trips >= 1.0 && matches!(gated, None | Some(0.0));
            if !ok {
                eprintln!("[repo_breaker] metrics not settled yet: trips={trips}, gated={gated:?}");
            }
            Ok(ok.then_some(()))
        },
    )
    .await
    .expect(
        "kopiur_repository_breaker_trips_total must count the trip and \
         kopiur_snapshot_gated must be absent-or-0 after recovery",
    );

    // Cleanup (best-effort; the shard tears the cluster down anyway).
    let _ = schedules.delete(SCHEDULE, &DeleteParams::default()).await;
    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    let _ = repos.delete(REPO, &DeleteParams::default()).await;
    let _ = repos.delete(ALERT_REPO, &DeleteParams::default()).await;
}
