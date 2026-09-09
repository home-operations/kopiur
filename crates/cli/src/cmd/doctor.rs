//! `kubectl kopiur doctor` — diagnose an installation: CRDs, operator and
//! webhook health (including a live admission probe), repository readiness,
//! credential Secrets, blocked/stuck work, recent failures, and recent
//! warnings.
//!
//! The checks themselves live in [`kopiur_ops::doctor`] (shared with the web
//! UI); this module is the command surface: flags → [`DoctorParams`], then the
//! report rendered as text/JSON/YAML with the exit code it implies.

use chrono::{DateTime, Utc};
use kopiur_ops::OpsError;
use kopiur_ops::doctor::{DoctorParams, DoctorReport, Outcome, run_all};

use crate::cli::DoctorArgs;
use crate::context::KubeCtx;
use crate::error::CliError;
use crate::output::OutputFormat;

/// Render the human report. Pure.
pub fn render(report: &DoctorReport) -> String {
    let mut out = String::new();
    for result in &report.checks {
        let line = match &result.outcome {
            Outcome::Pass => format!("  ok    {}\n", result.check.title()),
            Outcome::Warn(msg) => format!("  warn  {}: {}\n", result.check.title(), msg),
            Outcome::Fail { what, why, fix } => format!(
                "  FAIL  {}: {}\n        why: {}\n        fix: {}\n",
                result.check.title(),
                what,
                why,
                fix
            ),
        };
        out.push_str(&line);
    }
    let failed = report
        .checks
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Fail { .. }))
        .count();
    let warned = report
        .checks
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Warn(_)))
        .count();
    out.push_str(&format!(
        "\n{} check(s): {} failed, {} warning(s)\n",
        report.checks.len(),
        failed,
        warned
    ));
    out
}

/// Run all checks in order.
pub async fn run(
    ctx: &KubeCtx,
    args: &DoctorArgs,
    output: OutputFormat,
    now: DateTime<Utc>,
) -> Result<crate::CmdOutput, CliError> {
    let params = DoctorParams {
        stuck_threshold: args.stuck_threshold,
        failure_lookback: args.failure_lookback,
        // The CLI does not know where the operator runs; doctor finds its
        // Deployments by the chart's component labels across all namespaces.
        operator_namespace: None,
        // `kubectl kopiur doctor` diagnoses the whole installation, so it runs
        // every check. The subset exists for a caller that has one question and
        // pays for the answer on every page view — see `DoctorParams::checks`.
        checks: None,
    };
    let report = run_all(ctx, &params, now).await;
    let exit = report.exit_code();
    let text = match output {
        OutputFormat::Table | OutputFormat::Wide => render(&report),
        OutputFormat::Yaml => {
            let value = serde_json::to_value(&report).map_err(|e| OpsError::Serialization {
                what: "doctor report",
                source: e.into(),
            })?;
            serde_yaml::to_string(&value).map_err(|e| OpsError::Serialization {
                what: "doctor report",
                source: e.into(),
            })?
        }
        OutputFormat::Json => {
            let mut s =
                serde_json::to_string_pretty(&report).map_err(|e| OpsError::Serialization {
                    what: "doctor report",
                    source: e.into(),
                })?;
            s.push('\n');
            s
        }
        OutputFormat::Name => {
            return Err(OpsError::Serialization {
                what: "doctor report as -o name (doctor is a report, not a resource; use -o json)",
                source: Box::new(std::io::Error::other("unsupported output format")),
            }
            .into());
        }
    };
    Ok(crate::CmdOutput { text, exit })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_ops::doctor::{CheckResult, DoctorCheck};

    fn report(outcomes: Vec<Outcome>) -> DoctorReport {
        DoctorReport {
            checks: outcomes
                .into_iter()
                .map(|outcome| CheckResult {
                    check: DoctorCheck::CrdsInstalled,
                    outcome,
                })
                .collect(),
        }
    }

    #[test]
    fn render_carries_what_why_fix_for_failures() {
        let text = render(&report(vec![
            Outcome::Pass,
            Outcome::Warn("cannot list deployments (RBAC)".into()),
            Outcome::Fail {
                what: "missing CRD(s): snapshots.kopiur.home-operations.com".into(),
                why: "without the CRDs the API server rejects every kopiur object".into(),
                fix: "install kopiur".into(),
            },
        ]));
        assert!(text.contains("ok    CRDs installed"), "{text}");
        assert!(
            text.contains("warn  CRDs installed: cannot list deployments"),
            "{text}"
        );
        assert!(
            text.contains("FAIL  CRDs installed: missing CRD(s)"),
            "{text}"
        );
        assert!(text.contains("why: without the CRDs"), "{text}");
        assert!(text.contains("fix: install kopiur"), "{text}");
        assert!(
            text.contains("3 check(s): 1 failed, 1 warning(s)"),
            "{text}"
        );
    }
}
