//! e2e: deadline-killed bootstrap/probe Jobs (#413, #414, #415).
//!
//! One incident, three bugs: a cold-cache `kopia repository connect` outgrew
//! the 120s bootstrap Job deadline, and the operator (a) misreported the kill
//! as `BackendUnreachable` (#414), (b) withheld maintenance — the cure — from
//! the resulting `Degraded` repository (#413), and (c) relaunched the doomed
//! discovery Job every ~2.5 minutes forever, billed per request (#415).
//!
//! The fixture pairs an impossible `activeDeadlineSeconds: 1` with a network
//! BLACKHOLE of the MinIO port ([`kopiur_e2e::blackhole_tcp_port`]): the
//! connect hangs (SYN retransmits, no RST) so the kill is DETERMINISTICALLY
//! result-less. The blackhole is essential — against a healthy in-cluster
//! MinIO the whole mover run frequently beats the Job controller's lazy
//! deadline enforcement and persists a SUCCESS result, which rightly outranks
//! the Job's late `DeadlineExceeded` verdict (observed live while building
//! this test: the repo healed to Ready every probe cycle). Lifting the
//! blackhole then lets the deadline-escalation self-heal story complete.
//!
//! One sequential test fn on purpose: the blackhole is node-global state, so
//! two concurrently-running scenarios would fight over it — and this binary
//! must own its CI shard (every MinIO consumer hangs while the rule stands).
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use k8s_openapi::api::batch::v1::Job;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::{Api, ResourceExt};

use kopiur_api::{Maintenance, Repository};
use kopiur_e2e::{
    E2E_NAMESPACE, Need, World, blackhole_tcp_port, consts, default_timeout, poll_interval,
    wait_until,
};

/// The MinIO port the blackhole drops (`consts::MINIO_ENDPOINT`'s port).
const MINIO_PORT: u16 = 9000;

/// How long the #415 guard watches for a premature relaunch. The strict-retry
/// holdoff gates the second attempt for ≥120s from the failure's `lastProbeAt`
/// stamp; on the pre-fix code the relaunch landed within seconds of the
/// finalize, so 90s of "no new Job UID" cleanly separates the two without
/// waiting out a full backoff rung (the full 120→240→480→960→1800 ladder is
/// pinned by the unit sequence test
/// `recycled_bootstrap_failures_rearm_the_holdoff`).
const HOLDOFF_GUARD_WINDOW: Duration = Duration::from_secs(90);

fn repository_json(name: &str, bucket: &str, deadline_secs: Option<i64>) -> serde_json::Value {
    let mut spec = serde_json::json!({
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
        // failureThreshold 1: the FIRST deadline kill crosses the breaker
        // threshold, so the reason/condition assertions don't wait out a
        // 3-failure debounce.
        "health": { "probe": { "enabled": true, "interval": "30s", "failureThreshold": 1 } },
        // Yearly crons: a fresh Maintenance still fires ONE initial full run
        // immediately (`mode_after` anchors a year back when there is no run
        // history; full subsumes quick) — the test drains it before
        // blackholing — but no scheduled slot can then RE-fire mid-test and
        // grab the G3 single-flight slot the manual-run assertion depends on.
        // With the default quick cron (`0 */6 * * *`) a run straddling a
        // 6-hour boundary lost that race in CI: the scheduled Job hung
        // against the blackhole (48h mover deadline) and the manual run
        // starved behind it.
        "maintenance": { "schedule": {
            "quick": { "cron": "0 3 1 1 *" },
            "full": { "cron": "0 3 1 1 *" }
        } }
    });
    if let Some(secs) = deadline_secs {
        spec["bootstrap"] =
            serde_json::json!({ "failurePolicy": { "activeDeadlineSeconds": secs } });
    }
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Repository",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": spec
    })
}

fn status_value<K: serde::Serialize>(obj: &K) -> serde_json::Value {
    serde_json::to_value(obj)
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

/// Delete a leftover CR of the same name and wait for it to go (reused clusters).
async fn clear_leftover<K>(api: &Api<K>, name: &str)
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
    <K as kube::Resource>::DynamicType: Default,
{
    if api.get_opt(name).await.expect("query leftover").is_some() {
        let _ = api.delete(name, &DeleteParams::default()).await;
        wait_until(
            &format!("leftover {name} is gone"),
            default_timeout(),
            poll_interval(),
            || async { Ok(api.get_opt(name).await?.is_none().then_some(())) },
        )
        .await
        .expect("leftover CR should delete");
    }
}

/// Sample the `<name>-discovery` Job's uid every [`poll_interval`] for `window`,
/// returning every DISTINCT uid seen (the `health_probe.rs` churn-guard pattern:
/// under-sampling can only make the guard MORE forgiving, never flakily red).
async fn distinct_discovery_job_uids(
    jobs: &Api<Job>,
    job_name: &str,
    window: Duration,
) -> BTreeSet<String> {
    let deadline = Instant::now() + window;
    let mut seen = BTreeSet::new();
    while Instant::now() < deadline {
        if let Ok(Some(job)) = jobs.get_opt(job_name).await
            && let Some(uid) = job.uid()
        {
            seen.insert(uid);
        }
        tokio::time::sleep(poll_interval()).await;
    }
    seen
}

/// Restore the blackhole on ANY exit — a panicking assertion unwinds past the
/// in-line `blackhole_tcp_port(.., false)` call, and a leaked rule wedges the
/// retry (and every later scenario on a kept cluster) at MinIO provisioning.
/// Sync `docker` in `Drop` because `Drop` cannot await.
struct BlackholeGuard;
impl Drop for BlackholeGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args([
                "exec",
                consts::KIND_CONTROL_PLANE_CONTAINER,
                "iptables",
                "-w",
                "-D",
                "FORWARD",
                "-p",
                "tcp",
                "--dport",
                "9000",
                "-j",
                "DROP",
            ])
            .output();
    }
}

/// The whole deadline-kill arc, sequentially (the blackhole is node-global):
///
/// 1. `repo-a` bootstraps normally to Ready (+ managed Maintenance).
/// 2. Blackhole MinIO; drop `repo-a`'s deadline to 1s; birth `repo-b` under a
///    1s deadline. Every connect now hangs → every kill is result-less.
/// 3. #414: `repo-a` (strict relaunch of a bootstrapped repo) degrades with
///    `ProbeDeadlineExceeded`/`BootstrapDeadlineExceeded` and the
///    raise-the-deadline message — never the credentials-ghost text.
/// 4. #413: while `repo-a` is Degraded-by-deadline, its managed Maintenance is
///    NOT deferred `WaitingForRepository`; a manual run's Job spawns.
/// 5. #415: `repo-b`'s first kill stamps `consecutiveProbeFailures: 1` and no
///    new discovery Job appears inside the holdoff window.
/// 6. Heal (lift the blackhole): the manual maintenance run completes,
///    `repo-b`'s released second attempt carries the ESCALATED 2s deadline,
///    and both repositories self-heal to Ready — the end-to-end #414 story.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn deadline_kills_classify_exempt_maintenance_hold_off_and_self_heal() {
    let Some(world) = World::connect().await else {
        return;
    };
    // BEFORE provisioning: a rule leaked by an interrupted earlier try would
    // hang `ensure(Minio)`'s bucket pod. `-D` of an absent rule errors; ignore.
    let _ = blackhole_tcp_port(MINIO_PORT, false).await;
    let _guard = BlackholeGuard;
    world
        .ensure(&[Need::Minio])
        .await
        .expect("provision MinIO + buckets");
    let client = world.client().clone();
    let repo_a = "e2e-deadline-a";
    let repo_b = "e2e-deadline-b";
    let job_b = format!("{repo_b}-discovery");

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let maints: Api<Maintenance> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    clear_leftover(&repos, repo_a).await;
    clear_leftover(&repos, repo_b).await;

    // 1. Bootstrap `repo-a` normally (default 120s deadline, healthy MinIO).
    repos
        .create(
            &PostParams::default(),
            &serde_json::from_value(repository_json(repo_a, "kopiur-bootstrap-deadline-a", None))
                .expect("Repository JSON deserializes"),
        )
        .await
        .expect("create repo-a");
    wait_until(
        &format!("{repo_a} Ready with a uniqueId"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_value(&repos.get(repo_a).await?);
            let ready = s.get("phase").and_then(|p| p.as_str()) == Some("Ready")
                && s.get("uniqueId").and_then(|u| u.as_str()).is_some();
            Ok(ready.then_some(()))
        },
    )
    .await
    .expect("repo-a bootstraps to Ready");
    // Drain the projected Maintenance's initial scheduled run BEFORE the
    // blackhole: `mode_after` treats a history-less mode as due immediately,
    // so a fresh Maintenance fires its first full run right away regardless
    // of cron. That Job shares the G3 single-flight slot with the manual run
    // asserted in step 4 — if it is still in flight when the blackhole goes
    // up it hangs (its deadline is the 48h mover default) and the manual run
    // can never be accepted (the deterministic CI failure this wait fixes;
    // locally the initial run happened to finish before the blackhole). The
    // drained-state marker mirrors `mode_after`'s own anchor per mode:
    // `lastRunAt` (the mover's END-of-run stamp on success — a full run
    // stamps quick's too, full subsumes quick, so no separate quick Job ever
    // fires) or `lastHandledAt` (the controller's marker for a yielded run).
    // With the yearly crons above, nothing re-fires after this drain.
    wait_until(
        &format!("managed Maintenance {repo_a} drains its initial scheduled run"),
        default_timeout(),
        poll_interval(),
        || async {
            let Some(m) = maints.get_opt(repo_a).await? else {
                return Ok(None);
            };
            let s = status_value(&m);
            let anchored = |mode: &str| {
                s.pointer(&format!("/{mode}/lastRunAt")).is_some()
                    || s.pointer(&format!("/{mode}/lastHandledAt")).is_some()
            };
            Ok((anchored("full") && anchored("quick")).then_some(()))
        },
    )
    .await
    .expect("the managed Maintenance must finish its initial scheduled run pre-blackhole");

    // 2. Blackhole MinIO, then make both repos' deadlines impossible. From here
    //    to the heal, EVERY guard below must not leave this fn without lifting
    //    the rule — panics unwind past it, so a failed run leaves the rule for
    //    the next run's pre-clean above (and CI clusters are throwaway).
    blackhole_tcp_port(MINIO_PORT, true)
        .await
        .expect("install the MinIO blackhole");
    repos
        .patch(
            repo_a,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "spec": { "bootstrap": { "failurePolicy": { "activeDeadlineSeconds": 1 } } }
            })),
        )
        .await
        .expect("patch repo-a activeDeadlineSeconds");
    repos
        .create(
            &PostParams::default(),
            &serde_json::from_value(repository_json(
                repo_b,
                "kopiur-bootstrap-deadline-b",
                Some(1),
            ))
            .expect("Repository JSON deserializes"),
        )
        .await
        .expect("create repo-b");

    // 3. #414 on the bootstrapped repo: the strict relaunch is deadline-killed
    //    result-less and must degrade with the DEADLINE reasons.
    wait_until(
        &format!("{repo_a} Degraded with the deadline reasons"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_value(&repos.get(repo_a).await?);
            let classified = s.get("phase").and_then(|p| p.as_str()) == Some("Degraded")
                && condition(&s, "BackendReachable", "reason").as_deref()
                    == Some("ProbeDeadlineExceeded")
                && condition(&s, "Bootstrapped", "reason").as_deref()
                    == Some("BootstrapDeadlineExceeded");
            Ok(classified.then_some(()))
        },
    )
    .await
    .expect(
        "#414: a deadline kill must degrade with ProbeDeadlineExceeded/BootstrapDeadlineExceeded",
    );
    let s = status_value(&repos.get(repo_a).await.unwrap());
    let msg = condition(&s, "BackendReachable", "message").unwrap_or_default();
    assert!(
        msg.contains("activeDeadlineSeconds"),
        "#414: the alert must name the deadline and its fix, got: {msg}"
    );
    assert!(
        !msg.contains("credentials/lock failed"),
        "#414: the alert must NOT send the operator chasing credential ghosts: {msg}"
    );

    // 4. #413, asserted while `repo-a` is Degraded and the backend is STILL
    //    blackholed: the manual maintenance run must not defer
    //    WaitingForRepository, and its mover Job must spawn (it hangs against
    //    the blackhole for now — its own deadline is the 48h mover default —
    //    and completes after the heal below). The pre-blackhole drain above
    //    guarantees the G3 single-flight slot is FREE here: acceptance is
    //    gated on `has_active_maintenance_job`, so any lingering scheduled
    //    Job would starve this wait without the gate being at fault.
    let requested = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    maints
        .patch(
            repo_a,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "metadata": { "annotations": {
                kopiur_api::consts::RUN_REQUESTED_ANNOTATION: requested,
                kopiur_api::consts::RUN_MODE_ANNOTATION: "quick",
            }}})),
        )
        .await
        .expect("annotate Maintenance with a manual run request");
    // The #413 deadlock, as a live assertion: the G7 gate runs BEFORE manual-run
    // handling, so on the old Ready-only gate this wait can never complete — the
    // Maintenance parks at LeaseOwned=False/WaitingForRepository and the
    // annotation is never picked up. (Deliberately NOT asserted by sighting that
    // condition: a stale WaitingForRepository from the bootstrap-race window can
    // linger on status after the gate already passes, so only the manual run's
    // acceptance proves the gate's LIVE verdict.)
    wait_until(
        &format!("manual maintenance {requested} spawns against the Degraded repo-a"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_value(&maints.get(repo_a).await?);
            Ok(
                (s.pointer("/manualRun/requestedAt").and_then(|v| v.as_str())
                    == Some(requested.as_str()))
                .then_some(()),
            )
        },
    )
    .await
    .unwrap_or_else(|e| {
        panic!(
            "#413 deadlock: the manual maintenance run was never accepted against a \
             Degraded-because-slow repository — the G7 gate is deferring the cure: {e}"
        )
    });

    // 5a. #415 prerequisite on the never-bootstrapped repo: the first
    //     result-less kill stamps the failure streak and the deadline reason.
    wait_until(
        &format!("{repo_b} Degraded with consecutiveProbeFailures=1"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_value(&repos.get(repo_b).await?);
            let armed = s.get("phase").and_then(|p| p.as_str()) == Some("Degraded")
                && s.pointer("/health/consecutiveProbeFailures")
                    .and_then(|v| v.as_i64())
                    == Some(1);
            Ok(armed.then_some(()))
        },
    )
    .await
    .expect("#415: the first result-less failure must stamp the failure streak");
    let s = status_value(&repos.get(repo_b).await.unwrap());
    assert_eq!(
        condition(&s, "Ready", "reason").as_deref(),
        Some("BootstrapDeadlineExceeded"),
        "#414: a first-bootstrap deadline kill carries its own reason; status: {s}"
    );
    let msg = condition(&s, "Ready", "message").unwrap_or_default();
    assert!(
        msg.contains("activeDeadlineSeconds"),
        "#414: the message must name the deadline and its fix: {msg}"
    );
    let first_uids = distinct_discovery_job_uids(&jobs, &job_b, Duration::from_secs(5)).await;

    // 5b. THE #415 ASSERTION: for the guard window, NO new discovery Job may
    //     appear — the holdoff gates the relaunch for ≥120s from the failure.
    //     Pre-fix, the recycle route never stamped the holdoff's anchor, so a
    //     fresh Job (a fresh billed cold-cache connect) landed within seconds.
    let during = distinct_discovery_job_uids(&jobs, &job_b, HOLDOFF_GUARD_WINDOW).await;
    let new_during: BTreeSet<_> = during.difference(&first_uids).collect();
    assert!(
        new_during.is_empty(),
        "#415 relaunch metronome: {} new discovery Job(s) inside the {HOLDOFF_GUARD_WINDOW:?} \
         holdoff window — the failure streak/anchor is not gating the relaunch. New UIDs: \
         {new_during:?}",
        new_during.len()
    );

    // 6. HEAL: lift the blackhole. From here everything self-resolves.
    blackhole_tcp_port(MINIO_PORT, false)
        .await
        .expect("lift the MinIO blackhole");

    // 6a. The gate releases (no wedge) and the second attempt runs with the
    //     ESCALATED 2s deadline — the operator applied the raise-the-deadline
    //     remediation itself.
    wait_until(
        &format!("{job_b} second attempt with the escalated deadline"),
        default_timeout(),
        poll_interval(),
        || async {
            let Some(job) = jobs.get_opt(&job_b).await? else {
                return Ok(None);
            };
            if job.uid().is_none_or(|u| during.contains(&u)) {
                return Ok(None);
            }
            Ok(job.spec.as_ref().and_then(|s| s.active_deadline_seconds))
        },
    )
    .await
    .map(|deadline| {
        assert_eq!(
            deadline, 2,
            "deadline escalation: the second attempt must run with base<<1 = 2s"
        );
    })
    .expect("the holdoff must release and relaunch with an escalated deadline");

    // 6b. The manual maintenance run completes against the healed backend —
    //     proof the #413 exemption spawned REAL, working maintenance.
    wait_until(
        &format!("manual maintenance {requested} succeeds"),
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_value(&maints.get(repo_a).await?);
            let done = s.pointer("/manualRun/requestedAt").and_then(|v| v.as_str())
                == Some(requested.as_str())
                && s.pointer("/manualRun/phase").and_then(|v| v.as_str()) == Some("Succeeded");
            Ok(done.then_some(()))
        },
    )
    .await
    .expect("#413: the exempted maintenance run must succeed once the backend heals");

    // 6c. Both repositories self-heal to Ready: an escalated-deadline connect
    //     succeeds, the streak clears, and the deadline returns to base — the
    //     end-to-end #414 story (no human ever edited the spec back).
    for repo in [repo_a, repo_b] {
        wait_until(
            &format!("{repo} self-heals to Ready"),
            // The retry that heals rides the holdoff ladder, and repo-a's 4s
            // rung can race a slow pod start: a heal may need the NEXT rung,
            // waiting out backoff(2) = 480s first — budget for one full extra
            // rung beyond that.
            Duration::from_secs(900),
            poll_interval(),
            || async {
                let s = status_value(&repos.get(repo).await?);
                Ok((s.get("phase").and_then(|p| p.as_str()) == Some("Ready")).then_some(()))
            },
        )
        .await
        .unwrap_or_else(|e| {
            panic!("{repo} must self-heal to Ready after the blackhole lifts: {e}")
        });
    }

    // Cleanup (best-effort).
    let _ = repos.delete(repo_a, &DeleteParams::default()).await;
    let _ = repos.delete(repo_b, &DeleteParams::default()).await;
}
