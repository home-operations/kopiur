//! `kubectl kopiur suspend|resume <kind> <name>` — toggle the declarative
//! suspend field (ADR-0005 §14(e)) on any kind that has one.
//!
//! The kind routing and the patch live in [`kopiur_ops::suspend`]; this module
//! renders the resulting report for the requested `-o` format.

use kopiur_ops::OpsError;
use kopiur_ops::suspend::SuspendReport;

use crate::cli::SuspendArgs;
use crate::context::KubeCtx;
use crate::error::CliError;
use crate::output::OutputFormat;

/// Render the report for the requested output format. Pure.
pub fn render(report: &SuspendReport, output: OutputFormat) -> Result<String, CliError> {
    let resource = format!(
        "{}.{}/{}",
        report.meta.singular,
        kopiur_api::GROUP,
        report.name
    );
    match output {
        OutputFormat::Table | OutputFormat::Wide => {
            let verb = if report.desired {
                "suspended"
            } else {
                "resumed"
            };
            if report.previous == report.desired {
                Ok(format!("{resource} unchanged (already {verb})\n"))
            } else {
                Ok(format!("{resource} {verb}\n"))
            }
        }
        OutputFormat::Yaml => serde_yaml::to_string(&report.object).map_err(|e| {
            CliError::Ops(OpsError::Serialization {
                what: "patched object",
                source: e.into(),
            })
        }),
        OutputFormat::Json => {
            let mut s = serde_json::to_string_pretty(&report.object).map_err(|e| {
                CliError::Ops(OpsError::Serialization {
                    what: "patched object",
                    source: e.into(),
                })
            })?;
            s.push('\n');
            Ok(s)
        }
        OutputFormat::Name => Ok(format!("{resource}\n")),
    }
}

/// Entry point for both `suspend` (desired=true) and `resume` (desired=false).
pub async fn run(
    ctx: &KubeCtx,
    args: &SuspendArgs,
    desired: bool,
    output: OutputFormat,
) -> Result<String, CliError> {
    if matches!(ctx.scope, crate::context::Scope::All) {
        return Err(CliError::AllNamespacesNotApplicable {
            command: if desired { "suspend" } else { "resume" },
        });
    }
    let report = kopiur_ops::suspend::set_suspended(
        ctx,
        args.kind.into(),
        Some(ctx.namespace.as_str()),
        &args.name,
        desired,
    )
    .await?;
    render(&report, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_ops::suspend::{SuspendableKind, kind_meta};

    #[test]
    fn snapshot_replication_kind_meta_and_name_render_roundtrip() {
        let meta = kind_meta(SuspendableKind::SnapshotReplication);
        assert_eq!(meta.kind, "SnapshotReplication");
        assert_eq!(meta.singular, "snapshotreplication");
        assert_eq!(meta.plural, "snapshotreplications");
        let r = SuspendReport {
            meta,
            kind: meta.kind,
            name: "offsite".into(),
            namespace: Some("media".into()),
            previous: false,
            desired: true,
            object: serde_json::json!({ "kind": "SnapshotReplication" }),
        };
        assert_eq!(
            render(&r, OutputFormat::Name).unwrap(),
            "snapshotreplication.kopiur.home-operations.com/offsite\n"
        );
        assert_eq!(
            render(&r, OutputFormat::Table).unwrap(),
            "snapshotreplication.kopiur.home-operations.com/offsite suspended\n"
        );
    }

    fn report(previous: bool, desired: bool) -> SuspendReport {
        SuspendReport {
            meta: kind_meta(SuspendableKind::Policy),
            kind: "SnapshotPolicy",
            name: "nightly".into(),
            namespace: Some("media".into()),
            previous,
            desired,
            object: serde_json::json!({"kind": "SnapshotPolicy"}),
        }
    }

    #[test]
    fn table_render_states_the_transition_or_noop() {
        let changed = render(&report(false, true), OutputFormat::Table).unwrap();
        assert_eq!(
            changed,
            "snapshotpolicy.kopiur.home-operations.com/nightly suspended\n"
        );
        let resumed = render(&report(true, false), OutputFormat::Table).unwrap();
        assert_eq!(
            resumed,
            "snapshotpolicy.kopiur.home-operations.com/nightly resumed\n"
        );
        let noop = render(&report(true, true), OutputFormat::Table).unwrap();
        assert_eq!(
            noop,
            "snapshotpolicy.kopiur.home-operations.com/nightly unchanged (already suspended)\n"
        );
    }

    #[test]
    fn name_render_matches_kubectl_o_name() {
        let out = render(&report(false, true), OutputFormat::Name).unwrap();
        assert_eq!(out, "snapshotpolicy.kopiur.home-operations.com/nightly\n");
    }

    #[test]
    fn yaml_and_json_render_the_object_verbatim() {
        let r = report(false, true);
        let yaml = render(&r, OutputFormat::Yaml).unwrap();
        assert!(yaml.contains("kind: SnapshotPolicy"));
        let json = render(&r, OutputFormat::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["kind"], "SnapshotPolicy");
    }
}
