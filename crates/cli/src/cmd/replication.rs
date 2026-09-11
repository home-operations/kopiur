//! `kubectl kopiur replication run` — trigger an out-of-band replication run
//! by stamping the `run-requested` annotation; the operator routes it through
//! the SAME mover/gate/single-flight path as the cron slots and answers in
//! `status.manualRun`.
//!
//! The command covers BOTH replication kinds (`RepositoryReplication`,
//! `SnapshotReplication`) behind one verb, because "run my replication now" is
//! one intent — the kind is a detail of which object holds the schedule. It is
//! auto-detected from the name when unambiguous, and `--kind` settles the case
//! where a namespace has one of each under the same name.
//!
//! The decisions and the kube calls live in [`kopiur_ops::replication`]; this
//! module is the `--wait` loop and the text the command prints.

use chrono::{DateTime, Utc};
use kopiur_api::{RepositoryReplication, SnapshotReplication};
use kopiur_ops::replication::{
    ReplicationKind, ReplicationTarget, answered, detect_kind, failure_detail, request_run_by_kind,
};
use kube::api::Api;

use crate::CmdOutput;
use crate::cli::ReplicationRunArgs;
use crate::context::KubeCtx;
use crate::error::CliError;
use crate::wait::{DEFAULT_WAIT_TIMEOUT, wait_for};

/// Run `replication run`: resolve the target kind, stamp the annotation, and
/// optionally wait for `status.manualRun` to answer.
pub async fn run(
    ctx: &KubeCtx,
    args: &ReplicationRunArgs,
    now: DateTime<Utc>,
) -> Result<CmdOutput, CliError> {
    if matches!(ctx.scope, crate::context::Scope::All) {
        return Err(CliError::AllNamespacesNotApplicable {
            command: "replication run",
        });
    }
    // An explicit kind targets it directly; an omitted one is auto-detected
    // from what actually exists.
    let kind = match args.kind {
        Some(arg) => ReplicationKind::from(arg),
        None => detect_kind(ctx, &args.name).await?,
    };
    // The request itself is kind-routed by the shared layer, so the CLI has no
    // second copy of that dispatch to drift from it.
    let requested_at = request_run_by_kind(ctx, kind, &args.name, now).await?;
    // Exhaustive over the kind: a third replication CRD cannot compile until
    // this dispatch names it too. What remains kind-generic here is the
    // reporting and the `--wait` loop, which are CLI-only.
    match kind {
        ReplicationKind::RepositoryReplication => {
            report::<RepositoryReplication>(ctx, args, &requested_at).await
        }
        ReplicationKind::SnapshotReplication => {
            report::<SnapshotReplication>(ctx, args, &requested_at).await
        }
    }
}

/// The kind-generic body after the request landed: print the request line, then
/// optionally wait for `status.manualRun` to answer it.
async fn report<K: ReplicationTarget>(
    ctx: &KubeCtx,
    args: &ReplicationRunArgs,
    requested_at: &str,
) -> Result<CmdOutput, CliError> {
    let ns = ctx.namespace.as_str();
    let name = args.name.as_str();

    let requested_line = format!(
        "{}.{}/{name} run requested ({requested_at})\n",
        K::SINGULAR,
        kopiur_api::GROUP
    );
    if !args.wait {
        return Ok(CmdOutput::ok(requested_line));
    }

    eprint!("{requested_line}");
    let api: Api<K> = Api::namespaced(ctx.client.clone(), ns);
    let timeout = args.timeout.unwrap_or(DEFAULT_WAIT_TIMEOUT);
    let requested_for_check = requested_at.to_string();
    let verdict = wait_for(
        &api,
        name,
        format!("{} {name} requested run", K::KIND),
        format!(
            "watch it with `kubectl get {} {name} -n {ns} -o jsonpath='{{.status.manualRun}}'`, \
             or raise --timeout. A phase of Pending means the replication is suspended, or \
             the request is waiting behind an in-flight run — `kubectl kopiur resume` it if \
             suspended, or wait for the in-flight run to finish",
            K::PLURAL
        ),
        timeout,
        move |o: &K| answered(o, &requested_for_check),
    )
    .await;

    match verdict? {
        Ok(done) => {
            let completed = done
                .manual_run()
                .and_then(|m| m.completed_at.clone())
                .unwrap_or_default();
            Ok(CmdOutput::ok(format!(
                "{} {name} run completed at {completed}\n",
                K::KIND
            )))
        }
        Err(failed) => {
            eprint!("{}", failure_detail(failed.as_ref(), name, requested_at));
            Ok(CmdOutput {
                text: String::new(),
                exit: 1,
            })
        }
    }
}
