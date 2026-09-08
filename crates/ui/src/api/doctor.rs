//! `GET /api/v1/doctor` — the same checks `kubectl kopiur doctor` runs, served
//! out of `kopiur_ops::doctor` so the CLI and the UI can never disagree about
//! whether a cluster is healthy.
//!
//! # It runs as the caller, and it never fails
//!
//! Every check goes through the caller's impersonated client, so a namespace-
//! scoped user gets the report their own permissions can support — `run_all`
//! degrades a check it may not perform to a `Warn` naming the missing grant
//! rather than aborting. The endpoint therefore always answers 200 with a
//! report; a red check is data, not an error.

use axum::extract::{Query, State};
use axum::{Json, Router, routing::get};
use chrono::Utc;
use serde::Deserialize;
use std::time::Duration;

use kopiur_ops::doctor::{DoctorCheck, DoctorParams, DoctorReport, Outcome, run_all};
use kopiur_ui_model::views::{DoctorCheckView, DoctorReportView};

use crate::AppState;
use crate::api::problem::{ApiError, problem};
use crate::api::{client_for, ops_ctx};
use crate::auth::CurrentIdentity;

/// How long a `Snapshot`/`Restore` may sit non-terminal before doctor calls it
/// stuck, when the caller does not say. Matches the CLI's own default.
const DEFAULT_STUCK_THRESHOLD: Duration = Duration::from_secs(60 * 60);
/// How far back a terminal failure counts as a current problem, by default.
const DEFAULT_FAILURE_LOOKBACK: Duration = Duration::from_secs(24 * 60 * 60);
/// The longest window either knob may name — a year, past which the check is
/// asking about history rather than health.
const MAX_WINDOW_SECONDS: u64 = 365 * 24 * 60 * 60;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new().route("/doctor", get(handler))
}

/// The two windows doctor's verdicts depend on.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorQuery {
    /// Seconds a `Snapshot`/`Restore` may be non-terminal before it is stuck.
    #[serde(default)]
    pub stuck_threshold: Option<u64>,
    /// Seconds back a terminal failure still counts as current.
    #[serde(default)]
    pub failure_lookback: Option<u64>,
}

/// **Pure.** The stable identifier for one check — its variant name in
/// kebab-case, so `CrdsInstalled` becomes `crds-installed`.
///
/// Derived from the variant name rather than written out per variant: the
/// `Debug` name *is* the identifier, so the two cannot drift apart when a check
/// is added or renamed.
pub fn check_id(check: DoctorCheck) -> String {
    let name = format!("{check:?}");
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.char_indices() {
        if c.is_uppercase() {
            if i != 0 {
                out.push('-');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// **Pure.** Project one outcome onto the wire view.
///
/// Exhaustive over [`Outcome`]. Only a `Fail` carries what/why/fix: a `Warn`'s
/// single sentence is its whole story and splitting it into three would put
/// words in the check's mouth, and a `Pass` has nothing to say.
pub fn view_check(check: DoctorCheck, outcome: &Outcome) -> DoctorCheckView {
    let (label, what, why, fix) = match outcome {
        Outcome::Pass => ("Pass", None, None, None),
        Outcome::Warn(message) => ("Warn", Some(message.clone()), None, None),
        Outcome::Fail { what, why, fix } => (
            "Fail",
            Some(what.clone()),
            Some(why.clone()),
            Some(fix.clone()),
        ),
    };
    DoctorCheckView {
        check: check_id(check),
        title: check.title().to_string(),
        outcome: label.to_string(),
        what,
        why,
        fix,
    }
}

/// **Pure.** The whole report.
pub fn view_report(report: &DoctorReport, ran_at: &str) -> DoctorReportView {
    DoctorReportView {
        checks: report
            .checks
            .iter()
            .map(|c| view_check(c.check, &c.outcome))
            .collect(),
        exit_code: report.exit_code(),
        ran_at: ran_at.to_string(),
    }
}

/// **Pure.** Validate a window the caller named.
fn window(value: Option<u64>, default: Duration, field: &str) -> Result<Duration, ApiError> {
    match value {
        None => Ok(default),
        Some(0) => Err(problem(
            400,
            "invalid-filter",
            format!("`{field}` must be a positive number of seconds."),
            "Zero would mean every piece of work is stuck the instant it starts, which reports \
             a healthy cluster as broken.",
            format!("drop {field} to use the default, or give it a positive value"),
        )),
        Some(seconds) if seconds > MAX_WINDOW_SECONDS => Err(problem(
            400,
            "invalid-filter",
            format!("`{field}` is longer than a year."),
            "A window that long asks about history rather than about whether the cluster is \
             healthy right now, and every check would report everything it has ever seen.",
            format!("use a {field} of at most {MAX_WINDOW_SECONDS} seconds"),
        )),
        Some(seconds) => Ok(Duration::from_secs(seconds)),
    }
}

/// `GET /api/v1/doctor?stuckThreshold=&failureLookback=`
async fn handler(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    Query(q): Query<DoctorQuery>,
) -> Result<Json<DoctorReportView>, ApiError> {
    let params = DoctorParams {
        stuck_threshold: window(q.stuck_threshold, DEFAULT_STUCK_THRESHOLD, "stuckThreshold")?,
        failure_lookback: window(
            q.failure_lookback,
            DEFAULT_FAILURE_LOOKBACK,
            "failureLookback",
        )?,
        // An in-cluster server knows where the operator runs, which saves doctor
        // the label-driven search across every namespace the CLI has to do.
        operator_namespace: app.cfg.operator_namespace.clone(),
    };

    let client = client_for(&app, &id)?;
    // Cluster-wide: doctor's question is about the whole installation, and a
    // caller who may not see part of it gets that check degraded to a Warn.
    let ctx = ops_ctx(&app.cfg, client, None);
    let now = Utc::now();
    let report = run_all(&ctx, &params, now).await;
    Ok(Json(view_report(&report, &now.to_rfc3339())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_ops::doctor::CheckResult;

    #[test]
    fn a_check_id_is_its_variant_name_in_kebab_case() {
        assert_eq!(check_id(DoctorCheck::CrdsInstalled), "crds-installed");
        assert_eq!(
            check_id(DoctorCheck::ControllerRunning),
            "controller-running"
        );
        assert_eq!(check_id(DoctorCheck::NoStuckWork), "no-stuck-work");
        assert_eq!(
            check_id(DoctorCheck::SnapshotReplications),
            "snapshot-replications"
        );
    }

    #[test]
    fn a_pass_says_nothing_and_a_warn_says_one_thing() {
        let pass = view_check(DoctorCheck::CrdsInstalled, &Outcome::Pass);
        assert_eq!(pass.outcome, "Pass");
        assert_eq!(pass.check, "crds-installed");
        assert_eq!(pass.title, "CRDs installed");
        assert_eq!(pass.what, None);
        assert_eq!(pass.why, None);
        assert_eq!(pass.fix, None);

        let warn = view_check(
            DoctorCheck::WebhookRunning,
            &Outcome::Warn("cannot list deployments (RBAC)".into()),
        );
        assert_eq!(warn.outcome, "Warn");
        assert_eq!(
            warn.what.as_deref(),
            Some("cannot list deployments (RBAC)"),
            "a warning's one sentence is its whole story"
        );
        assert_eq!(warn.why, None, "splitting a warning would invent words");
        assert_eq!(warn.fix, None);
    }

    #[test]
    fn a_failure_carries_the_whole_what_why_fix_triple() {
        let fail = view_check(
            DoctorCheck::RepositoriesReady,
            &Outcome::Fail {
                what: "Repository media/nas is Failed".into(),
                why: "its password Secret is missing".into(),
                fix: "create the Secret nas-pw in namespace media".into(),
            },
        );
        assert_eq!(fail.outcome, "Fail");
        assert_eq!(fail.what.as_deref(), Some("Repository media/nas is Failed"));
        assert_eq!(fail.why.as_deref(), Some("its password Secret is missing"));
        assert_eq!(
            fail.fix.as_deref(),
            Some("create the Secret nas-pw in namespace media")
        );
    }

    #[test]
    fn the_reports_exit_code_is_the_one_the_cli_would_have_returned() {
        let all_good = DoctorReport {
            checks: vec![CheckResult {
                check: DoctorCheck::CrdsInstalled,
                outcome: Outcome::Pass,
            }],
        };
        assert_eq!(view_report(&all_good, "2026-09-08T12:00:00Z").exit_code, 0);

        let warned = DoctorReport {
            checks: vec![CheckResult {
                check: DoctorCheck::CrdsInstalled,
                outcome: Outcome::Warn("could not verify".into()),
            }],
        };
        assert_eq!(
            view_report(&warned, "2026-09-08T12:00:00Z").exit_code,
            0,
            "a warning is not a failure"
        );

        let failed = DoctorReport {
            checks: vec![
                CheckResult {
                    check: DoctorCheck::CrdsInstalled,
                    outcome: Outcome::Pass,
                },
                CheckResult {
                    check: DoctorCheck::NoStuckWork,
                    outcome: Outcome::Fail {
                        what: "a Snapshot is parked".into(),
                        why: "MoverPermitted=False".into(),
                        fix: "annotate the namespace".into(),
                    },
                },
            ],
        };
        let view = view_report(&failed, "2026-09-08T12:00:00Z");
        assert_eq!(view.exit_code, 1);
        assert_eq!(view.checks.len(), 2, "every check that ran is reported");
        assert_eq!(view.ran_at, "2026-09-08T12:00:00Z");
    }

    #[test]
    fn the_windows_default_and_are_bounded() {
        assert_eq!(
            window(None, DEFAULT_STUCK_THRESHOLD, "stuckThreshold").unwrap(),
            DEFAULT_STUCK_THRESHOLD
        );
        assert_eq!(
            window(Some(300), DEFAULT_STUCK_THRESHOLD, "stuckThreshold").unwrap(),
            Duration::from_secs(300)
        );

        let zero = window(Some(0), DEFAULT_STUCK_THRESHOLD, "stuckThreshold").unwrap_err();
        assert_eq!(zero.0.status, 400);
        assert!(zero.0.fix.contains("stuckThreshold"), "got {}", zero.0.fix);

        let huge = window(
            Some(MAX_WINDOW_SECONDS + 1),
            DEFAULT_FAILURE_LOOKBACK,
            "failureLookback",
        )
        .unwrap_err();
        assert_eq!(huge.0.status, 400);
        assert!(huge.0.what.contains("year"), "got {}", huge.0.what);
    }
}
