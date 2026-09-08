//! `kubectl kopiur restore` — the one-liner over the `Restore` CRD: exactly
//! one source (snapshot / policy / raw identity) into exactly one target
//! (created PVC / existing PVC / populator), with optional wait + log stream.
//!
//! The CR-building and terminal-phase classification live in
//! [`kopiur_ops::actions::restore`], shared with the web UI; the clap flags are
//! mapped onto its `RestoreRequest` by the conversion in [`crate::cli`]. What
//! stays here is the CLI's own surface: the `--wait`/`--logs` loop and the
//! rendering.

use chrono::{DateTime, Utc};
use kopiur_api::Restore;
use kopiur_ops::actions::restore::{
    RestoreRequest, build_restore, create_restore, failure_detail, success_summary, terminal,
};
use kube::api::Api;

use crate::CmdOutput;
use crate::cli::RestoreArgs;
use crate::context::KubeCtx;
use crate::error::CliError;
use crate::output::OutputFormat;
use crate::wait::{DEFAULT_WAIT_TIMEOUT, wait_for};

/// Run `restore`.
pub async fn run(
    ctx: &KubeCtx,
    args: &RestoreArgs,
    output: OutputFormat,
    now: DateTime<Utc>,
) -> Result<CmdOutput, CliError> {
    if matches!(ctx.scope, crate::context::Scope::All) {
        return Err(CliError::AllNamespacesNotApplicable { command: "restore" });
    }
    let ns = ctx.namespace.as_str();
    let restores: Api<Restore> = Api::namespaced(ctx.client.clone(), ns);
    // Total: clap's `source`/`target` ArgGroups already refused every other
    // shape (see the conversion in `crate::cli`).
    let req = RestoreRequest::from(args);
    let restore = build_restore(&req, ns, now);
    let name = restore.metadata.name.clone().expect("name set by builder");
    let created = create_restore(ctx, ns, restore).await?;

    let wait = args.wait || args.logs;
    let created_line = format!("restore.{}/{} created\n", kopiur_api::GROUP, name);
    if !wait {
        let text = match output {
            OutputFormat::Table | OutputFormat::Wide => created_line,
            OutputFormat::Yaml => {
                // Through a JSON Value first: serde_yaml would render the
                // externally-tagged enums (source/target) as `!snapshotRef`
                // YAML tags — not the cluster's encoding (convention #5).
                let value =
                    serde_json::to_value(&created).map_err(|e| CliError::Serialization {
                        what: "created Restore",
                        source: e.into(),
                    })?;
                serde_yaml::to_string(&value).map_err(|e| CliError::Serialization {
                    what: "created Restore",
                    source: e.into(),
                })?
            }
            OutputFormat::Json => {
                let mut s = serde_json::to_string_pretty(&created).map_err(|e| {
                    CliError::Serialization {
                        what: "created Restore",
                        source: e.into(),
                    }
                })?;
                s.push('\n');
                s
            }
            OutputFormat::Name => format!("restore.{}/{}\n", kopiur_api::GROUP, name),
        };
        return Ok(CmdOutput { text, exit: 0 });
    }

    eprint!("{created_line}");
    let log_task = if args.logs {
        let ctx_clone = ctx.clone();
        let restore_name = name.clone();
        Some(tokio::spawn(async move {
            crate::cmd::logs::stream_target_logs_when_ready(
                &ctx_clone,
                crate::cmd::logs::LogsTarget::Restore,
                &restore_name,
            )
            .await
        }))
    } else {
        None
    };

    let timeout = args.timeout.unwrap_or(DEFAULT_WAIT_TIMEOUT);
    let verdict = wait_for(
        &restores,
        &name,
        format!("restore {name}"),
        format!(
            "follow it with `kubectl kopiur logs restore {name} -n {ns} -f`, or raise --timeout"
        ),
        timeout,
        terminal,
    )
    .await;

    if let Some(mut task) = log_task
        && tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
            .await
            .is_err()
    {
        task.abort();
    }

    match verdict? {
        Ok(completed) => Ok(CmdOutput {
            text: success_summary(&completed),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use chrono::TimeZone;
    use clap::Parser;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 11, 3, 0, 12).unwrap()
    }

    /// Parse a full command line and extract the RestoreArgs.
    fn parse(args: &[&str]) -> RestoreArgs {
        let cli = Cli::try_parse_from(
            ["kubectl-kopiur", "restore"]
                .into_iter()
                .chain(args.iter().copied()),
        )
        .unwrap_or_else(|e| panic!("args {args:?} should parse: {e}"));
        match cli.command {
            Command::Restore(a) => *a,
            other => panic!("expected restore, got {other:?}"),
        }
    }

    fn parse_err(args: &[&str]) -> clap::Error {
        Cli::try_parse_from(
            ["kubectl-kopiur", "restore"]
                .into_iter()
                .chain(args.iter().copied()),
        )
        .expect_err("should not parse")
    }

    #[test]
    fn missing_source_or_target_fails_at_parse_time() {
        let err = parse_err(&["--to-pvc", "x"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let err = parse_err(&["--from-snapshot", "s"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let err = parse_err(&[
            "--from-snapshot",
            "s",
            "--from-policy",
            "p",
            "--to-pvc",
            "x",
        ]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let err = parse_err(&["--from-snapshot", "s", "--to-pvc", "x", "--populator"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn create_pvc_requires_an_explicit_size() {
        // The webhook refuses a created PVC without capacity; fail at parse
        // time with flag-level wording instead.
        let err = parse_err(&["--from-policy", "p", "--create-pvc", "x"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    /// `--source-path` (#443 review wave 2, finding 3b): the per-PVC path
    /// override, fromPolicy only. Any other source is a parse-time error, not a
    /// silently dropped flag.
    #[test]
    fn source_path_is_refused_with_other_sources() {
        for source in [
            &["--from-snapshot", "snap1"][..],
            &["--identity", "u@h:/data", "--repository", "r"][..],
        ] {
            let mut argv = source.to_vec();
            argv.extend(["--source-path", "/pvc/x", "--to-pvc", "d"]);
            let err = parse_err(&argv);
            assert!(
                err.to_string().contains("--source-path"),
                "{source:?}: {err}"
            );
        }
    }

    #[test]
    fn snapshot_id_excludes_point_in_time_selectors() {
        let err = parse_err(&[
            "--identity",
            "u@h",
            "--repository",
            "r",
            "--snapshot-id",
            "abc",
            "--as-of",
            "2026-01-01T00:00:00Z",
            "--to-pvc",
            "x",
        ]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn identity_requires_repository_and_as_of_conflicts_with_snapshot_ref() {
        let err = parse_err(&["--identity", "u@h", "--to-pvc", "x"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let err = parse_err(&[
            "--from-snapshot",
            "s",
            "--as-of",
            "2026-01-01T00:00:00Z",
            "--to-pvc",
            "x",
        ]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn identity_value_parses_user_host_and_optional_path() {
        let args = parse(&[
            "--identity",
            "pg@media:/pvc/data",
            "--repository",
            "nas",
            "--populator",
        ]);
        let id = args.identity.unwrap();
        assert_eq!(id.username, "pg");
        assert_eq!(id.hostname, "media");
        assert_eq!(id.source_path.as_deref(), Some("/pvc/data"));

        let args = parse(&[
            "--identity",
            "pg@media",
            "--repository",
            "nas",
            "--populator",
        ]);
        assert_eq!(args.identity.unwrap().source_path, None);

        for bad in ["nohost", "@h", "u@", "u@h:"] {
            let err = parse_err(&["--identity", bad, "--repository", "nas", "--populator"]);
            assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation, "{bad}");
        }
    }

    #[test]
    fn yaml_output_uses_the_cluster_encoding_not_serde_yaml_tags() {
        // serde_yaml renders newtype enum variants as `!snapshotRef` tags when
        // serialized directly; the CLI must emit the cluster's plain-mapping
        // encoding (via a JSON Value) so the output is kubectl-applyable.
        let args = parse(&["--from-snapshot", "snap1", "--to-pvc", "data"]);
        let req = RestoreRequest::from(&args);
        let restore = build_restore(&req, "media", at());
        let value = serde_json::to_value(&restore).unwrap();
        let yaml = serde_yaml::to_string(&value).unwrap();
        assert!(yaml.contains("snapshotRef:"), "{yaml}");
        assert!(yaml.contains("pvcRef:"), "{yaml}");
        assert!(
            !yaml.contains('!'),
            "no serde_yaml enum tags allowed: {yaml}"
        );
        // The direct serialization WOULD tag — this guards the reason the
        // Value route exists. If serde_yaml ever changes, revisit.
        let direct = serde_yaml::to_string(&restore).unwrap();
        assert!(
            direct.contains('!'),
            "serde_yaml behavior changed: {direct}"
        );
    }
}
