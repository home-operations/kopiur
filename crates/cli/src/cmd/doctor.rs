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
use kopiur_ops::doctor::{DoctorParams, DoctorReport, Outcome, Severity, doctor_scope, run_all};

use crate::cli::DoctorArgs;
use crate::context::{KubeCtx, Scope};
use crate::error::CliError;
use crate::output::OutputFormat;

/// The word the text report prints for one severity. Exhaustive over the
/// shared [`Severity`] the console's lamp also reads, so the two surfaces
/// cannot colour the same check differently (#695).
pub fn severity_word(severity: Severity) -> &'static str {
    match severity {
        Severity::Pass => "ok",
        Severity::Warn => "warn",
        Severity::Fail => "FAIL",
    }
}

/// How the summary line names what the run read.
fn scope_phrase(scope: &Scope) -> String {
    match scope {
        Scope::All => "every namespace".to_string(),
        Scope::Namespace(ns) => format!("namespace {ns}"),
    }
}

/// Render the human report. Pure.
pub fn render(report: &DoctorReport, scope: &Scope) -> String {
    let mut out = String::new();
    for result in &report.checks {
        let word = severity_word(result.outcome.severity());
        let line = match &result.outcome {
            Outcome::Pass => format!("  {word:<5} {}\n", result.check.title()),
            Outcome::Warn(msg) => format!("  {word:<5} {}: {}\n", result.check.title(), msg),
            Outcome::Fail { what, why, fix } => format!(
                "  {word:<5} {}: {}\n        why: {}\n        fix: {}\n",
                result.check.title(),
                what,
                why,
                fix
            ),
        };
        out.push_str(&line);
    }
    let count = |severity: Severity| {
        report
            .checks
            .iter()
            .filter(|c| c.outcome.severity() == severity)
            .count()
    };
    out.push_str(&format!(
        "\n{} check(s): {} failed, {} warning(s) (scope: {})\n",
        report.checks.len(),
        count(Severity::Fail),
        count(Severity::Warn),
        scope_phrase(scope)
    ));
    out
}

/// The context one doctor run reads through: the caller's, with its scope
/// replaced by [`doctor_scope`] of the namespace the user explicitly passed.
///
/// Doctor diagnoses the installation, so without `-n` it reads every namespace
/// — the kubeconfig context's namespace is NOT a request to narrow it (that
/// default is what made `kubectl kopiur doctor` report `ok` for failures the
/// console showed, #695). The context's own `namespace` is left alone: the
/// admission probe dry-runs into it.
pub fn doctor_ctx(ctx: &KubeCtx, explicit_namespace: Option<&str>) -> KubeCtx {
    let mut ctx = ctx.clone();
    ctx.scope = doctor_scope(explicit_namespace);
    ctx
}

/// Run all checks in order.
pub async fn run(
    ctx: &KubeCtx,
    explicit_namespace: Option<&str>,
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
    let ctx = doctor_ctx(ctx, explicit_namespace);
    let report = run_all(&ctx, &params, now).await;
    let exit = report.exit_code();
    let text = match output {
        OutputFormat::Table | OutputFormat::Wide => render(&report, &ctx.scope),
        OutputFormat::Yaml => {
            let value = serde_json::to_value(&report).map_err(|e| OpsError::Serialization {
                what: "doctor report",
                source: e.into(),
            })?;
            crate::output::to_yaml(&value).map_err(|e| OpsError::Serialization {
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
                .map(|outcome| CheckResult::new(DoctorCheck::CrdsInstalled, outcome))
                .collect(),
        }
    }

    #[test]
    fn render_carries_what_why_fix_for_failures() {
        let text = render(
            &report(vec![
                Outcome::Pass,
                Outcome::Warn("cannot list deployments (RBAC)".into()),
                Outcome::Fail {
                    what: "missing CRD(s): snapshots.kopiur.home-operations.com".into(),
                    why: "without the CRDs the API server rejects every kopiur object".into(),
                    fix: "install kopiur".into(),
                },
            ]),
            &Scope::All,
        );
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
            text.contains("3 check(s): 1 failed, 1 warning(s) (scope: every namespace)"),
            "{text}"
        );
    }

    /// A context pointed at a closed port; only its scope is read here.
    fn ctx_in(namespace: &str, scope: Scope) -> KubeCtx {
        KubeCtx {
            client: kube::Client::try_from(kube::Config::new(
                "http://127.0.0.1:1/".parse().expect("a literal URL parses"),
            ))
            .expect("a Config with no auth builds a Client"),
            namespace: namespace.to_string(),
            scope,
            field_manager: "kopiur-test".to_string(),
        }
    }

    /// #695: `kubectl kopiur doctor` with a kubeconfig context namespace of
    /// `kopiur-system` examined only that namespace, so it printed `ok` for the
    /// failures and warnings the console (cluster-wide) showed everywhere
    /// else. The context namespace must not narrow doctor; only `-n` does.
    #[tokio::test]
    async fn doctor_reads_every_namespace_unless_one_is_named_explicitly() {
        // `connect` scoped the context to the kubeconfig namespace...
        let defaulted = ctx_in("kopiur-system", Scope::Namespace("kopiur-system".into()));
        // ...and doctor widens it back, keeping the context namespace (the
        // admission probe dry-runs into it).
        let ctx = doctor_ctx(&defaulted, None);
        assert_eq!(ctx.scope, Scope::All);
        assert_eq!(ctx.namespace, "kopiur-system");

        let narrowed = doctor_ctx(&defaulted, Some("media"));
        assert_eq!(narrowed.scope, Scope::Namespace("media".into()));
    }

    #[test]
    fn the_summary_names_the_scope_it_read() {
        let text = render(
            &report(vec![Outcome::Pass]),
            &Scope::Namespace("media".into()),
        );
        assert!(text.contains("(scope: namespace media)"), "{text}");
    }
}
