//! `kubectl kopiur snapshot now` — run a SnapshotPolicy immediately by
//! creating a manual `Snapshot` CR, optionally waiting for the terminal phase
//! (and streaming the mover's logs along the way).
//!
//! The CR-building, fan-out planning and terminal-phase classification live in
//! [`kopiur_ops::actions::snapshot`], shared with the web UI; what stays here
//! is the CLI's own surface: the `--wait`/`--logs` loop and the rendering.

use chrono::{DateTime, Utc};
use kopiur_api::{Snapshot, SnapshotPolicy};
use kopiur_ops::OpsError;
use kopiur_ops::actions::snapshot::{
    SnapshotNowRequest, create_snapshots, failure_detail, plan_snapshots, success_summary, terminal,
};
use kube::api::Api;

use crate::CmdOutput;
use crate::cli::SnapshotNowArgs;
use crate::context::KubeCtx;
use crate::error::{CliError, classify_kube};
use crate::output::OutputFormat;
use crate::wait::{DEFAULT_WAIT_TIMEOUT, wait_for};

/// Run `snapshot now`.
pub async fn run(
    ctx: &KubeCtx,
    args: &SnapshotNowArgs,
    output: OutputFormat,
    now: DateTime<Utc>,
) -> Result<CmdOutput, CliError> {
    if matches!(ctx.scope, crate::context::Scope::All) {
        return Err(CliError::AllNamespacesNotApplicable {
            command: "snapshot now",
        });
    }
    let ns = ctx.namespace.as_str();
    let req = SnapshotNowRequest::from(args);

    // Preflight: the recipe must exist (actionable miss beats a webhook 404
    // chain), and a suspended recipe deserves a heads-up (the run proceeds —
    // the operator is authoritative about what suspension means).
    let policies: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), ns);
    let policy = policies.get(&args.policy).await.map_err(|e| {
        classify_kube(
            "get",
            "SnapshotPolicy",
            "snapshotpolicies",
            Some(ns),
            Some(&args.policy),
            e,
        )
    })?;
    if policy.spec.suspend {
        eprintln!(
            "warning: SnapshotPolicy {} is suspended; the operator may not run this snapshot until it is resumed \
             (kubectl kopiur resume policy {})",
            args.policy, args.policy
        );
    }

    let snapshots: Api<Snapshot> = Api::namespaced(ctx.client.clone(), ns);
    // A `pvcSelector` recipe expands to one Snapshot per matched PVC (#346).
    // The planner does the expansion itself for the same reason a
    // SnapshotSchedule does: a Snapshot must never guess which of N volumes it
    // covers.
    let planned = plan_snapshots(ctx, &req, &policy, ns, now).await?;
    let created_all = create_snapshots(ctx, ns, &planned).await?;
    // The single-CR paths below (wait / --logs / the object echo) follow the
    // FIRST child. With a fan-out there is no single object to echo, so the
    // names are listed instead and the wait covers every child.
    let created = created_all.first().cloned().expect("at least one");
    let names: Vec<String> = created_all
        .iter()
        .filter_map(|s| s.metadata.name.clone())
        .collect();
    let name = names.first().cloned().expect("at least one");
    let fanned = names.len() > 1;

    let wait = args.wait || args.logs;
    let created_line = if fanned {
        format!(
            "{} snapshots created ({}) — SnapshotPolicy {} expands a pvcSelector\n",
            names.len(),
            names.join(", "),
            args.policy
        )
    } else {
        format!("snapshot.{}/{} created\n", kopiur_api::GROUP, name)
    };
    if !wait {
        let text = match output {
            OutputFormat::Table | OutputFormat::Wide => created_line,
            OutputFormat::Yaml => {
                // Via a JSON Value: keeps the cluster's encoding for any
                // externally-tagged enum (see cmd/restore.rs; SnapshotSpec has
                // none today, but the route must not depend on that).
                let value = serde_json::to_value(&created).map_err(|e| {
                    CliError::Ops(OpsError::Serialization {
                        what: "created Snapshot",
                        source: e.into(),
                    })
                })?;
                serde_yaml::to_string(&value).map_err(|e| {
                    CliError::Ops(OpsError::Serialization {
                        what: "created Snapshot",
                        source: e.into(),
                    })
                })?
            }
            OutputFormat::Json => {
                let mut s = serde_json::to_string_pretty(&created).map_err(|e| {
                    CliError::Ops(OpsError::Serialization {
                        what: "created Snapshot",
                        source: e.into(),
                    })
                })?;
                s.push('\n');
                s
            }
            OutputFormat::Name => format!("snapshot.{}/{}\n", kopiur_api::GROUP, name),
        };
        return Ok(CmdOutput { text, exit: 0 });
    }

    // Waiting: progress goes to stderr so stdout stays the result (or the
    // mover logs when --logs).
    eprint!("{created_line}");
    let log_task = if args.logs {
        let ctx_clone = ctx.clone();
        let snap_name = name.clone();
        Some(tokio::spawn(async move {
            crate::cmd::logs::stream_target_logs_when_ready(
                &ctx_clone,
                crate::cmd::logs::LogsTarget::Snapshot,
                &snap_name,
            )
            .await
        }))
    } else {
        None
    };

    let timeout = args.timeout.unwrap_or(DEFAULT_WAIT_TIMEOUT);
    let verdict = wait_for(
        &snapshots,
        &name,
        format!("snapshot {name}"),
        format!(
            "follow it with `kubectl kopiur logs snapshot {name} -n {ns} -f`, or raise --timeout"
        ),
        timeout,
        terminal,
    )
    .await;

    // The streamer self-exits at the terminal phase once the final pod is
    // drained; give it a bounded moment, then abort — dropping the timeout
    // future alone would detach (not stop) the task.
    if let Some(mut task) = log_task
        && tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
            .await
            .is_err()
    {
        task.abort();
    }

    match verdict? {
        Ok(succeeded) => Ok(CmdOutput {
            text: success_summary(&succeeded),
            exit: 0,
        }),
        Err(failed) => {
            eprint!("{}", failure_detail(&failed));
            Ok(CmdOutput {
                text: String::new(),
                exit: 1,
            })
        }
    }
}
