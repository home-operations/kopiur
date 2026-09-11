//! `kubectl kopiur maintenance run` — trigger an out-of-band maintenance run
//! by stamping the `run-requested`/`run-mode` annotations; the operator routes
//! it through the SAME mover/lease/single-flight path as the cron slots and
//! answers in `status.manualRun`.
//!
//! The decisions and the two kube calls live in [`kopiur_ops::maintenance`];
//! this module is the `--wait` loop and the text the command prints.

use chrono::{DateTime, Utc};
use kopiur_api::common::RepositoryKind;
use kopiur_api::{Maintenance, ManualRunMode};
use kopiur_ops::maintenance::{MaintenanceTarget, answered, failure_detail, yield_note};
use kube::ResourceExt;
use kube::api::Api;

use crate::CmdOutput;
use crate::cli::MaintenanceRunArgs;
use crate::context::KubeCtx;
use crate::error::CliError;
use crate::wait::{DEFAULT_WAIT_TIMEOUT, wait_for};

/// Which Maintenance the flags name: an explicit NAME, else the repository
/// whose covering Maintenance is looked up. The clap ArgGroup makes exactly one
/// of them present.
fn target_of(args: &MaintenanceRunArgs) -> MaintenanceTarget {
    match &args.name {
        Some(name) => MaintenanceTarget::Named(name.clone()),
        None => MaintenanceTarget::ByRepository {
            kind: RepositoryKind::from(args.repository_kind),
            name: args.repository.clone().expect("clap group"),
        },
    }
}

/// Run `maintenance run`.
pub async fn run(
    ctx: &KubeCtx,
    args: &MaintenanceRunArgs,
    now: DateTime<Utc>,
) -> Result<CmdOutput, CliError> {
    if matches!(ctx.scope, crate::context::Scope::All) {
        return Err(CliError::AllNamespacesNotApplicable {
            command: "maintenance run",
        });
    }
    let maint = kopiur_ops::maintenance::resolve(ctx, &target_of(args)).await?;
    let name = maint.name_any();
    // Wait where the resolved Maintenance LIVES — the same namespace
    // `request_run` patches in (a ClusterRepository's managed Maintenance is
    // typically in the operator namespace, not the caller's).
    let maint_ns = maint
        .metadata
        .namespace
        .clone()
        .unwrap_or_else(|| ctx.namespace.clone());
    let ns = maint_ns.as_str();

    let mode = if args.full {
        ManualRunMode::Full
    } else {
        ManualRunMode::Quick
    };
    let requested_at = kopiur_ops::maintenance::request_run(ctx, &maint, mode, now).await?;

    let requested_line = format!(
        "maintenance.{}/{name} {} run requested ({requested_at})\n",
        kopiur_api::GROUP,
        mode.label()
    );
    if !args.wait {
        return Ok(CmdOutput::ok(requested_line));
    }

    eprint!("{requested_line}");
    let api: Api<Maintenance> = Api::namespaced(ctx.client.clone(), ns);
    let timeout = args.timeout.unwrap_or(DEFAULT_WAIT_TIMEOUT);
    let requested_for_check = requested_at.clone();
    let verdict = wait_for(
        &api,
        &name,
        format!("maintenance {name} manual run"),
        format!(
            "watch it with `kubectl get maintenance {name} -n {ns} -o jsonpath='{{.status.manualRun}}'`, \
             or raise --timeout"
        ),
        timeout,
        move |m: &Maintenance| answered(m, &requested_for_check),
    )
    .await;

    match verdict? {
        Ok(done) => {
            let completed = done
                .status
                .as_ref()
                .and_then(|s| s.manual_run.as_ref())
                .and_then(|m| m.completed_at.clone())
                .unwrap_or_default();
            // Honesty: a Job that YIELDED the lease succeeded without running
            // any maintenance — the user explicitly asked for a run, so say so.
            if let Some(note) = yield_note(&done) {
                eprintln!("{note}");
            }
            Ok(CmdOutput::ok(format!(
                "maintenance {name} {} run completed at {completed}\n",
                mode.label()
            )))
        }
        Err(failed) => {
            eprint!("{}", failure_detail(&failed, &requested_at));
            Ok(CmdOutput {
                text: String::new(),
                exit: 1,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::RepositoryKindArg;

    fn args(
        name: Option<&str>,
        repository: Option<&str>,
        kind: RepositoryKindArg,
    ) -> MaintenanceRunArgs {
        MaintenanceRunArgs {
            name: name.map(str::to_string),
            repository: repository.map(str::to_string),
            repository_kind: kind,
            full: false,
            wait: false,
            timeout: None,
        }
    }

    #[test]
    fn target_follows_the_flag_the_user_gave() {
        assert_eq!(
            target_of(&args(
                Some("nas-maint"),
                None,
                RepositoryKindArg::Repository
            )),
            MaintenanceTarget::Named("nas-maint".into())
        );
        assert_eq!(
            target_of(&args(
                None,
                Some("shared"),
                RepositoryKindArg::ClusterRepository
            )),
            MaintenanceTarget::ByRepository {
                kind: RepositoryKind::ClusterRepository,
                name: "shared".into(),
            }
        );
    }
}
