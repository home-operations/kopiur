//! `kubectl kopiur logs snapshot|restore <name>` — find the mover Job behind a
//! Snapshot/Restore and stream its pod logs, falling back to the status
//! `logTail` when the Job/pods are already gone.

use futures::{AsyncBufReadExt, TryStreamExt};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kopiur_api::consts::{OP_LABEL, OP_RESTORE};
use kopiur_api::{Restore, Snapshot};
use kube::ResourceExt;
use kube::api::{Api, ListParams, LogParams};

use crate::CmdOutput;
use crate::cli::LogsArgs;
use crate::context::KubeCtx;
use crate::error::{CliError, classify_kube};

/// Which CR kind the logs are for. Explicit subcommand — Snapshot and Restore
/// may share names, so the plugin never guesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogsTarget {
    /// A `Snapshot`'s backup mover Job.
    Snapshot,
    /// A `Restore`'s restore mover Job.
    Restore,
}

/// The upstream label every Job controller stamps on its pods.
const JOB_NAME_LABEL: &str = "batch.kubernetes.io/job-name";

/// Filter `jobs` to those owned (ownerReference) by the CR with `uid`,
/// then pick the newest. Pure.
pub fn newest_job_owned_by(jobs: Vec<Job>, uid: &str) -> Option<Job> {
    jobs.into_iter()
        .filter(|j| {
            j.metadata
                .owner_references
                .as_ref()
                .is_some_and(|refs| refs.iter().any(|r| r.uid == uid))
        })
        .max_by(|a, b| {
            a.metadata
                .creation_timestamp
                .cmp(&b.metadata.creation_timestamp)
        })
}

/// Newest pod of the list (a retried Job leaves several; the newest is the
/// current/last attempt). Pure.
pub fn newest_pod(pods: Vec<Pod>) -> Option<Pod> {
    pods.into_iter().max_by(|a, b| {
        a.metadata
            .creation_timestamp
            .cmp(&b.metadata.creation_timestamp)
    })
}

/// One status-recorded mover tail: the top-level `logTail`/`failure` of a
/// Snapshot or a direct Restore (`scope: None`), or ONE claim's own record of a
/// fanned-out populator Restore (`scope: Some(<pvc>)`, #443).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedTail {
    /// The claiming PVC this tail belongs to; `None` for the top-level fields.
    pub scope: Option<String>,
    /// `logTail` as recorded.
    pub log_tail: Option<String>,
    /// `failure` as recorded.
    pub failure: Option<kopiur_api::common::FailureBlock>,
}

/// A Snapshot's recorded tail: always the single top-level pair. Pure.
pub fn snapshot_recorded_tails(status: Option<&kopiur_api::SnapshotStatus>) -> Vec<RecordedTail> {
    vec![RecordedTail {
        scope: None,
        log_tail: status.and_then(|s| s.log_tail.clone()),
        failure: status.and_then(|s| s.failure.clone()),
    }]
}

/// A Restore's recorded tails (review wave 2, finding 8): PER CLAIM when the
/// populator fanned out (`status.claims` non-empty — each claim's mover writes
/// its own `logTail`/`failure` there, and the top-level pair is a direct
/// restore's), else the top-level pair. Pure.
pub fn restore_recorded_tails(status: Option<&kopiur_api::RestoreStatus>) -> Vec<RecordedTail> {
    match status.map(|s| &s.claims).filter(|c| !c.is_empty()) {
        Some(claims) => claims
            .iter()
            .map(|(pvc, claim)| RecordedTail {
                scope: Some(pvc.clone()),
                log_tail: claim.log_tail.clone(),
                failure: claim.failure.clone(),
            })
            .collect(),
        None => vec![RecordedTail {
            scope: None,
            log_tail: status.and_then(|s| s.log_tail.clone()),
            failure: status.and_then(|s| s.failure.clone()),
        }],
    }
}

/// The fallback text when the Job/pods have been garbage-collected: the
/// status-recorded tail(s) plus an honest note that full logs have rotated.
/// One section per [`RecordedTail`], headed by its claim when scoped. Pure.
pub fn gone_fallback(kind: &str, name: &str, tails: &[RecordedTail]) -> String {
    let mut out = format!(
        "the mover Job (and its pods) for {kind} {name} no longer exist — \
         completed Jobs are garbage-collected. Showing the tail recorded in status:\n"
    );
    let any_recorded = tails
        .iter()
        .any(|t| t.log_tail.is_some() || t.failure.is_some());
    if !any_recorded {
        out.push_str("(no logTail recorded — the run may never have started)\n");
        return out;
    }
    for t in tails {
        if let Some(pvc) = &t.scope {
            out.push_str(&format!("=== claim {pvc} ===\n"));
            if t.log_tail.is_none() && t.failure.is_none() {
                out.push_str("(no logTail recorded for this claim)\n");
                continue;
            }
        }
        if let Some(f) = &t.failure {
            out.push_str(&format!(
                "failure ({}): {}\n",
                f.kopia_error_class, f.message
            ));
            if let Some(stderr) = &f.stderr_tail {
                out.push_str(&format!("--- kopia stderr tail ---\n{stderr}\n"));
            }
        }
        if let Some(tail) = &t.log_tail {
            out.push_str(&format!("--- log tail ---\n{tail}\n"));
        }
    }
    out
}

/// Resolve the mover Job name for the target CR.
async fn resolve_job(
    ctx: &KubeCtx,
    target: LogsTarget,
    name: &str,
) -> Result<Result<String, String>, CliError> {
    let ns = ctx.namespace.as_str();
    match target {
        LogsTarget::Snapshot => {
            let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), ns);
            let snap = api.get(name).await.map_err(|e| {
                classify_kube("get", "Snapshot", "snapshots", Some(ns), Some(name), e)
            })?;
            // The controller records the Job name in status; fall back to an
            // ownerRef scan for status written by an older controller.
            if let Some(job) = snap
                .status
                .as_ref()
                .and_then(|s| s.job.as_ref())
                .and_then(|j| j.name.clone())
            {
                return Ok(Ok(job));
            }
            let uid = snap.uid().unwrap_or_default();
            let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), ns);
            let listed = jobs
                .list(&ListParams::default())
                .await
                .map_err(|e| classify_kube("list", "Job", "jobs", Some(ns), None, e))?;
            match newest_job_owned_by(listed.items, &uid) {
                Some(job) => Ok(Ok(job.name_any())),
                None => Ok(Err(gone_fallback(
                    "snapshot",
                    name,
                    &snapshot_recorded_tails(snap.status.as_ref()),
                ))),
            }
        }
        LogsTarget::Restore => {
            let api: Api<Restore> = Api::namespaced(ctx.client.clone(), ns);
            let restore = api.get(name).await.map_err(|e| {
                classify_kube("get", "Restore", "restores", Some(ns), Some(name), e)
            })?;
            let uid = restore.uid().unwrap_or_default();
            let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), ns);
            let listed = jobs
                .list(&ListParams::default().labels(&format!("{OP_LABEL}={OP_RESTORE}")))
                .await
                .map_err(|e| classify_kube("list", "Job", "jobs", Some(ns), None, e))?;
            match newest_job_owned_by(listed.items, &uid) {
                Some(job) => Ok(Ok(job.name_any())),
                // Per claim for a fanned-out populator (#443, wave 2 finding 8).
                None => Ok(Err(gone_fallback(
                    "restore",
                    name,
                    &restore_recorded_tails(restore.status.as_ref()),
                ))),
            }
        }
    }
}

/// Stream one pod's logs to stdout.
async fn stream_pod(ctx: &KubeCtx, pod: &str, params: &LogParams) -> Result<(), CliError> {
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), ctx.namespace.as_str());
    let stream = pods.log_stream(pod, params).await.map_err(|e| {
        classify_kube(
            "get",
            "Pod",
            "pods/log",
            Some(ctx.namespace.as_str()),
            Some(pod),
            e,
        )
    })?;
    let mut lines = stream.lines();
    while let Some(line) = lines
        .try_next()
        .await
        .map_err(|e| CliError::LogStreamInterrupted { source: e.into() })?
    {
        println!("{line}");
    }
    Ok(())
}

/// Run `logs snapshot|restore`.
pub async fn run(
    ctx: &KubeCtx,
    target: LogsTarget,
    args: &LogsArgs,
) -> Result<CmdOutput, CliError> {
    if matches!(ctx.scope, crate::context::Scope::All) {
        return Err(CliError::AllNamespacesNotApplicable { command: "logs" });
    }
    let job = match resolve_job(ctx, target, &args.name).await? {
        Ok(job) => job,
        Err(fallback) => {
            return Ok(CmdOutput {
                text: fallback,
                exit: 0,
            });
        }
    };
    let ns = ctx.namespace.as_str();
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), ns);
    let listed = pods
        .list(&ListParams::default().labels(&format!("{JOB_NAME_LABEL}={job}")))
        .await
        .map_err(|e| classify_kube("list", "Pod", "pods", Some(ns), None, e))?;
    let Some(pod) = newest_pod(listed.items) else {
        // The Job object survived but its pods rotated; same honest fallback.
        let (kind, tails) = fetch_tail(ctx, target, &args.name).await?;
        return Ok(CmdOutput {
            text: gone_fallback(kind, &args.name, &tails),
            exit: 0,
        });
    };
    let params = LogParams {
        follow: args.follow,
        previous: args.previous,
        tail_lines: args.tail,
        ..LogParams::default()
    };
    stream_pod(ctx, &pod.name_any(), &params).await?;
    Ok(CmdOutput {
        text: String::new(),
        exit: 0,
    })
}

/// Re-fetch the CR's recorded tail for the pods-gone fallback.
async fn fetch_tail(
    ctx: &KubeCtx,
    target: LogsTarget,
    name: &str,
) -> Result<(&'static str, Vec<RecordedTail>), CliError> {
    let ns = ctx.namespace.as_str();
    match target {
        LogsTarget::Snapshot => {
            let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), ns);
            let snap = api.get(name).await.map_err(|e| {
                classify_kube("get", "Snapshot", "snapshots", Some(ns), Some(name), e)
            })?;
            Ok(("snapshot", snapshot_recorded_tails(snap.status.as_ref())))
        }
        LogsTarget::Restore => {
            let api: Api<Restore> = Api::namespaced(ctx.client.clone(), ns);
            let restore = api.get(name).await.map_err(|e| {
                classify_kube("get", "Restore", "restores", Some(ns), Some(name), e)
            })?;
            Ok(("restore", restore_recorded_tails(restore.status.as_ref())))
        }
    }
}

/// Is the snapshot in a phase where no further mover pods can appear?
fn snapshot_terminal(snap: &Snapshot) -> bool {
    use kopiur_api::SnapshotPhase;
    match snap.status.as_ref().and_then(|s| s.phase.as_ref()) {
        Some(SnapshotPhase::Pending | SnapshotPhase::Running) | None => false,
        Some(
            SnapshotPhase::Succeeded
            | SnapshotPhase::Failed
            | SnapshotPhase::Deleting
            | SnapshotPhase::Discovered
            | SnapshotPhase::Unchanged,
        ) => true,
        // Never declare "no more pods can appear" on a phase this build cannot
        // read: `kubectl kopiur logs -f` keeps following instead of exiting
        // early on a run a newer operator is still driving.
        Some(SnapshotPhase::Unknown(_)) => false,
    }
}

/// Is the restore in a phase where no further mover pods can appear?
fn restore_terminal(restore: &Restore) -> bool {
    use kopiur_api::RestorePhase;
    match restore.status.as_ref().and_then(|s| s.phase.as_ref()) {
        Some(RestorePhase::Pending | RestorePhase::Resolving | RestorePhase::Restoring) | None => {
            false
        }
        Some(RestorePhase::Completed | RestorePhase::Failed) => true,
        // Keep following rather than exiting early — see `snapshot_terminal`.
        Some(RestorePhase::Unknown(_)) => false,
    }
}

/// One polling observation of the target CR for the background streamer:
/// gone / (terminal?, job name?).
enum Observation {
    Gone,
    Alive { terminal: bool, job: Option<String> },
}

/// Observe the target CR once: phase terminality + the mover Job, resolved the
/// same way the foreground `logs` command does it.
async fn observe(
    ctx: &KubeCtx,
    target: LogsTarget,
    name: &str,
) -> Result<Observation, kube::Error> {
    let ns = ctx.namespace.as_str();
    match target {
        LogsTarget::Snapshot => {
            let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), ns);
            let Some(snap) = api.get_opt(name).await? else {
                return Ok(Observation::Gone);
            };
            let job = snap
                .status
                .as_ref()
                .and_then(|st| st.job.as_ref())
                .and_then(|j| j.name.clone());
            Ok(Observation::Alive {
                terminal: snapshot_terminal(&snap),
                job,
            })
        }
        LogsTarget::Restore => {
            let api: Api<Restore> = Api::namespaced(ctx.client.clone(), ns);
            let Some(restore) = api.get_opt(name).await? else {
                return Ok(Observation::Gone);
            };
            let uid = restore.uid().unwrap_or_default();
            let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), ns);
            // A list ERROR must propagate (caller retries) — treating it as
            // "no job" while terminal would end the stream before the final
            // pod is drained.
            let listed = jobs
                .list(&ListParams::default().labels(&format!("{OP_LABEL}={OP_RESTORE}")))
                .await?;
            let job = newest_job_owned_by(listed.items, &uid).map(|j| j.name_any());
            Ok(Observation::Alive {
                terminal: restore_terminal(&restore),
                job,
            })
        }
    }
}

/// Best-effort companion for `snapshot now --logs` / `restore --logs`: wait
/// for the mover Job to exist, then follow the newest pod's logs to stdout.
/// Errors are retried (a just-created pod 400s `log_stream` until its
/// container starts — the common case, not a failure); only persistent ones
/// warn. The task exits on its own once the CR reaches a terminal phase and
/// the final pod's stream has been drained.
pub async fn stream_target_logs_when_ready(ctx: &KubeCtx, target: LogsTarget, name: &str) {
    const POLL: std::time::Duration = std::time::Duration::from_secs(2);
    // ~20s of consecutive stream failures before the user hears about it.
    const QUIET_FAILURES: u32 = 10;
    let ns = ctx.namespace.as_str();
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), ns);
    // A retried Job produces a new pod; keep following the newest unseen pod
    // until the CR is terminal and the last pod has been streamed.
    let mut streamed: Option<String> = None;
    let mut failures: u32 = 0;
    loop {
        let (terminal, job) = match observe(ctx, target, name).await {
            // Deleted mid-run: nothing further to stream.
            Ok(Observation::Gone) => return,
            Ok(Observation::Alive { terminal, job }) => (terminal, job),
            Err(e) => {
                tracing::debug!(error = %e, "observing CR for log streaming");
                tokio::time::sleep(POLL).await;
                continue;
            }
        };
        let Some(job) = job else {
            // Terminal with no Job recorded: there are no logs to stream.
            if terminal {
                return;
            }
            tokio::time::sleep(POLL).await;
            continue;
        };
        let pod = match pods
            .list(&ListParams::default().labels(&format!("{JOB_NAME_LABEL}={job}")))
            .await
        {
            Ok(listed) => newest_pod(listed.items).map(|p| p.name_any()),
            Err(e) => {
                tracing::debug!(error = %e, "listing mover pods for log streaming");
                None
            }
        };
        let Some(pod) = pod else {
            if terminal {
                return;
            }
            tokio::time::sleep(POLL).await;
            continue;
        };
        if streamed.as_deref() == Some(pod.as_str()) {
            // Newest pod already streamed to completion: done once terminal,
            // otherwise wait for a retry attempt's new pod.
            if terminal {
                return;
            }
            tokio::time::sleep(POLL).await;
            continue;
        }
        let params = LogParams {
            follow: true,
            ..LogParams::default()
        };
        match stream_pod(ctx, &pod, &params).await {
            Ok(()) => {
                // The follow stream ends when the container exits; loop to
                // catch a retry pod or finish at the terminal phase.
                streamed = Some(pod);
                failures = 0;
            }
            Err(e) => {
                // Most often "container is waiting to start": retry, don't quit.
                failures += 1;
                if failures == QUIET_FAILURES {
                    eprintln!(
                        "warning: still unable to stream mover logs from pod {pod} ({e}); retrying"
                    );
                }
                tokio::time::sleep(POLL).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(name: &str, owner_uid: Option<&str>, created: &str) -> Job {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": {
                "name": name,
                "namespace": "media",
                "creationTimestamp": created,
                "ownerReferences": owner_uid.map(|uid| vec![serde_json::json!({
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "Snapshot",
                    "name": "s",
                    "uid": uid
                })]),
            }
        }))
        .unwrap()
    }

    fn pod(name: &str, created: &str) -> Pod {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": name, "namespace": "media", "creationTimestamp": created }
        }))
        .unwrap()
    }

    #[test]
    fn newest_owned_job_wins_and_foreign_jobs_are_ignored() {
        let jobs = vec![
            job("attempt-1", Some("uid-1"), "2026-06-11T03:00:00Z"),
            job("attempt-2", Some("uid-1"), "2026-06-11T04:00:00Z"),
            job("other-cr", Some("uid-2"), "2026-06-11T05:00:00Z"),
            job("unowned", None, "2026-06-11T06:00:00Z"),
        ];
        let picked = newest_job_owned_by(jobs, "uid-1").unwrap();
        assert_eq!(picked.metadata.name.as_deref(), Some("attempt-2"));
        assert!(newest_job_owned_by(vec![], "uid-1").is_none());
    }

    #[test]
    fn newest_pod_picks_latest_attempt() {
        let pods = vec![
            pod("p-old", "2026-06-11T03:00:00Z"),
            pod("p-new", "2026-06-11T03:10:00Z"),
        ];
        assert_eq!(
            newest_pod(pods).unwrap().metadata.name.as_deref(),
            Some("p-new")
        );
    }

    #[test]
    fn gone_fallback_is_honest_about_rotation_and_shows_the_tail() {
        let failure = kopiur_api::common::FailureBlock {
            kopia_error_class: "AuthFailure".into(),
            message: "creds rejected".into(),
            stderr_tail: Some("denied".into()),
            exit_code: Some(1),
            retry_recommended: false,
            op: None,
        };
        let top = |tail: Option<&str>, failure: Option<&kopiur_api::common::FailureBlock>| {
            vec![RecordedTail {
                scope: None,
                log_tail: tail.map(str::to_string),
                failure: failure.cloned(),
            }]
        };
        let text = gone_fallback("snapshot", "s", &top(Some("tail lines"), Some(&failure)));
        assert!(text.contains("no longer exist"));
        assert!(text.contains("garbage-collected"));
        assert!(text.contains("failure (AuthFailure): creds rejected"));
        assert!(text.contains("--- log tail ---\ntail lines"));
        assert!(!text.contains("=== claim"), "top-level: no claim headers");

        let empty = gone_fallback("restore", "r", &top(None, None));
        assert!(empty.contains("no logTail recorded"));
    }

    /// Review wave 2, finding 8: `kubectl kopiur logs restore X` falls back to
    /// the CLAIMS' recorded tails for a fanned-out populator, one section per
    /// claim; a direct restore (no claims) keeps the top-level pair.
    #[test]
    fn restore_gone_fallback_shows_each_claims_own_tail() {
        let status: kopiur_api::RestoreStatus = serde_json::from_value(serde_json::json!({
            "phase": "Failed",
            "claims": {
                "data": { "phase": "Populated", "reason": "RestoreSucceeded",
                          "logTail": "data: restored 12 files" },
                "logs": {
                    "phase": "Failed", "reason": "MoverJobFailed",
                    "failure": { "kopiaErrorClass": "PermissionDenied",
                                 "message": "cannot write /data",
                                 "retryRecommended": false,
                                 "stderrTail": "ERROR permission denied" },
                    "logTail": "logs: restore failed"
                },
                "cache": { "phase": "Pending", "reason": "AwaitingPodSchedule" }
            }
        }))
        .unwrap();
        let tails = restore_recorded_tails(Some(&status));
        assert_eq!(
            tails.iter().map(|t| t.scope.as_deref()).collect::<Vec<_>>(),
            [Some("cache"), Some("data"), Some("logs")]
        );
        let text = gone_fallback("restore", "app", &tails);
        assert_eq!(
            text,
            "the mover Job (and its pods) for restore app no longer exist — completed Jobs \
             are garbage-collected. Showing the tail recorded in status:\n\
             === claim cache ===\n(no logTail recorded for this claim)\n\
             === claim data ===\n--- log tail ---\ndata: restored 12 files\n\
             === claim logs ===\nfailure (PermissionDenied): cannot write /data\n\
             --- kopia stderr tail ---\nERROR permission denied\n\
             --- log tail ---\nlogs: restore failed\n"
        );

        // A direct restore: the top-level pair, unscoped.
        let direct: kopiur_api::RestoreStatus = serde_json::from_value(serde_json::json!({
            "phase": "Failed", "logTail": "direct tail"
        }))
        .unwrap();
        let tails = restore_recorded_tails(Some(&direct));
        assert_eq!(tails.len(), 1);
        assert_eq!(tails[0].scope, None);
        assert_eq!(tails[0].log_tail.as_deref(), Some("direct tail"));
        // No status at all: one empty top-level entry ⇒ the honest note.
        assert!(
            gone_fallback("restore", "r", &restore_recorded_tails(None))
                .contains("no logTail recorded")
        );
    }
}
