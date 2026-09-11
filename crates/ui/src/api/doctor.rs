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
//!
//! # A caller can ask for less
//!
//! The full run is expensive — a `dryRun` create through the admission chain, a
//! Secret read per credential reference across the fleet, and a cluster-wide
//! Events list — which is too much for a dashboard that re-fetches whenever a
//! tab regains focus. `?checks=` names the subset to run, and the work behind
//! the checks left out is not done at all. The report then carries only the
//! rows that ran, so a consumer must count outcomes from `checks` rather than
//! assuming ten.

use axum::extract::State;
use axum::{Json, Router, routing::get};
use chrono::Utc;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::time::Duration;

use kopiur_ops::doctor::{DoctorCheck, DoctorParams, DoctorReport, Outcome, run_all};
use kopiur_ops::{OpsCtx, Scope};
use kopiur_ui_model::views::{DoctorCheckView, DoctorReportView, DoctorScopeView};

use crate::AppState;
use crate::api::problem::{ApiError, problem};
use crate::api::{UiQuery, client_for, ops_ctx};
use crate::auth::CurrentIdentity;
use crate::config::UiConfig;

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

/// The two windows doctor's verdicts depend on, and the namespace to scope them
/// to.
///
/// `deny_unknown_fields`: this query had no `namespace` and no denial, so
/// `?namespace=media` was accepted and ignored — a selector that silently does
/// nothing shows a green cluster while the namespace the user was actually
/// asking about burns. Now it either scopes the run or is a 400.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DoctorQuery {
    /// Seconds a `Snapshot`/`Restore` may be non-terminal before it is stuck.
    #[serde(default)]
    pub stuck_threshold: Option<u64>,
    /// Seconds back a terminal failure still counts as current.
    #[serde(default)]
    pub failure_lookback: Option<u64>,
    /// Restrict the checks that *can* be restricted to this namespace; absent
    /// runs cluster-wide. See [`doctor_ctx`] for which checks it moves and which
    /// stay installation-wide whatever this says.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Run only these checks, as a comma-separated list of the ids
    /// [`check_id`] produces (`repositories-ready,no-stuck-work`). Absent runs
    /// all ten, exactly as before this parameter existed.
    ///
    /// Comma-separated rather than repeated (`?checks=a&checks=b`) because
    /// `axum`'s `Query` deserializes with `serde_urlencoded`, which has no
    /// sequence support: a repeated key would silently keep only one value,
    /// which is precisely the shape of failure `deny_unknown_fields` was added
    /// to this query to prevent. [`parse_checks`] rejects an unrecognized id
    /// with a 400 naming every valid one.
    ///
    /// A caller that asks for a subset gets a **shorter report**, not a report
    /// with the rest passing — an absent row means "not run".
    #[serde(default)]
    pub checks: Option<String>,
}

/// The context one doctor run reads through.
///
/// `?namespace=` narrows the run's **scope**, not its context namespace, and
/// those are two different things here. Which checks it moves is published per
/// row as `DoctorCheckView.scope` — see [`check_scope`], which is the contract;
/// this comment is only orientation, and the prose that used to live here in
/// its place was wrong about two of the ten (it called the repository checks
/// namespace-scoped, and `list_repos` lists `ClusterRepository` cluster-wide).
///
/// Roughly: the work-backed checks and the events check narrow completely, the
/// three repository-backed ones narrow for `Repository` and not for
/// `ClusterRepository`, and the four installation checks do not narrow at all.
///
/// `webhook-admits` is why this is not simply `ops_ctx(cfg, client, namespace)`:
/// it dry-run-applies a `SnapshotPolicy` into `ctx.namespace` to see whether the
/// admission webhook answers. Moving that into the caller's namespace would make
/// a user without `create` there see "the webhook is not admitting" — a red
/// check about the installation, caused by their own RBAC. So the scope moves
/// and the namespace does not.
fn doctor_ctx(cfg: &UiConfig, client: kube::Client, namespace: Option<&str>) -> OpsCtx {
    scoped_for(ops_ctx(cfg, client, None), namespace)
}

/// **Pure.** Narrow a cluster-wide context to one namespace's *scope*, leaving
/// its `namespace` — the operator's — alone. See [`doctor_ctx`].
fn scoped_for(mut ctx: OpsCtx, namespace: Option<&str>) -> OpsCtx {
    if let Some(ns) = namespace {
        ctx.scope = Scope::Namespace(ns.to_string());
    }
    ctx
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

/// **Pure.** Every check id this endpoint accepts, comma-separated, for a
/// refusal. Derived from [`DoctorCheck::ALL`] through the same [`check_id`] the
/// report uses, so an id the parser accepts cannot be missing from the error
/// that lists them — and a check added later joins both at once.
fn accepted_check_ids() -> String {
    DoctorCheck::ALL
        .iter()
        .map(|c| check_id(*c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// **Pure.** Parse the `checks` selector. `None` in, `None` out — an absent
/// selector runs everything.
///
/// Every failure names the whole accepted vocabulary rather than only the
/// offending token: a caller who mistyped one id usually cannot see the list
/// anywhere else, and this query is `deny_unknown_fields` precisely so that a
/// selector which does not do what it says is a 400 rather than a quietly
/// broader report than the caller asked for.
fn parse_checks(raw: Option<&str>) -> Result<Option<BTreeSet<DoctorCheck>>, ApiError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut selected = BTreeSet::new();
    for token in raw.split(',') {
        let id = token.trim();
        if id.is_empty() {
            continue;
        }
        let found = DoctorCheck::ALL.iter().find(|c| check_id(**c) == id);
        match found {
            Some(check) => {
                selected.insert(*check);
            }
            None => {
                return Err(problem(
                    400,
                    "invalid-filter",
                    format!("`{id}` is not a doctor check."),
                    "`checks` selects which diagnostics to run, so an id nothing matches would \
                     quietly narrow the report — and a check missing from a report reads as one \
                     that passed.",
                    format!(
                        "use a comma-separated subset of: {}; or drop `checks` to run them all",
                        accepted_check_ids()
                    ),
                ));
            }
        }
    }
    if selected.is_empty() {
        return Err(problem(
            400,
            "invalid-filter",
            "`checks` names no check.".to_string(),
            "An empty selector would produce an empty report, which is indistinguishable from \
             a cluster with nothing wrong.",
            format!(
                "name at least one of: {}; or drop `checks` to run them all",
                accepted_check_ids()
            ),
        ));
    }
    Ok(Some(selected))
}

/// **Pure.** How much of the cluster one check actually reads.
///
/// Exhaustive over [`DoctorCheck`], so a check added later cannot ship without
/// stating its scope. Each arm is derived from the *reads the check performs*,
/// not from its title — which is the whole point. The client used to keep its
/// own two-way table beside a prose contract in this module's docs, and the
/// prose was a simplification: [`kopiur_ops::doctor::list_repos`] lists
/// `Repository` inside `?namespace=` but `ClusterRepository` cluster-wide, so
/// the UI printed "scoped to media" over two checks that also answer for every
/// other namespace. Those two are [`DoctorScopeView::Mixed`].
///
/// The three installation-wide checks read cluster-scoped objects (CRDs) or the
/// operator's own Deployments, found by chart labels across every namespace when
/// the server does not know where the operator runs; `webhook-admits` dry-runs
/// into the operator's namespace, never the caller's (see [`doctor_ctx`]). None
/// of the four moves when `?namespace=` does.
pub fn check_scope(check: DoctorCheck) -> DoctorScopeView {
    match check {
        // Reads `CustomResourceDefinition`, a cluster-scoped kind.
        DoctorCheck::CrdsInstalled => DoctorScopeView::Installation,
        // Reads the operator's own Deployment by chart labels — in
        // `operator_namespace` when the server knows it, across all namespaces
        // otherwise. Either way not the caller's namespace.
        DoctorCheck::ControllerRunning | DoctorCheck::WebhookRunning => {
            DoctorScopeView::Installation
        }
        // Dry-run-creates a SnapshotPolicy in the OPERATOR's namespace, so the
        // verdict is about the installation's webhook, never about the caller's
        // namespace or their `create` rights in it.
        DoctorCheck::WebhookAdmits => DoctorScopeView::Installation,
        // Both read `list_repos`: `Repository` narrowed by the scope, and
        // `ClusterRepository` always cluster-wide. `credentials-present` then
        // reads namespaced Secrets for whatever that returned — including the
        // Secrets a ClusterRepository pins in namespaces the caller did not ask
        // about.
        DoctorCheck::RepositoriesReady | DoctorCheck::CredentialsPresent => DoctorScopeView::Mixed,
        // Lists namespaced `SnapshotReplication`s, but resolves each one's
        // repository refs against `list_repos` — so a ref to a
        // `ClusterRepository` is judged on a cluster-scoped object.
        DoctorCheck::SnapshotReplications => DoctorScopeView::Mixed,
        // Both read `list_work`: Snapshot, Restore, SnapshotSchedule and
        // SnapshotPolicy, every one of them namespaced.
        DoctorCheck::NoStuckWork | DoctorCheck::RecentFailures => DoctorScopeView::Namespace,
        // Lists `Event`s, a namespaced kind.
        DoctorCheck::RecentWarnings => DoctorScopeView::Namespace,
    }
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
        scope: check_scope(check),
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

/// `GET /api/v1/doctor?stuckThreshold=&failureLookback=&namespace=&checks=`
async fn handler(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiQuery(q): UiQuery<DoctorQuery>,
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
        checks: parse_checks(q.checks.as_deref())?,
    };

    let client = client_for(&app, &id)?;
    // Cluster-wide unless the caller narrowed it: doctor's question is about the
    // whole installation, and a caller who may not see part of it gets that
    // check degraded to a Warn rather than an error.
    let ctx = doctor_ctx(&app.cfg, client, q.namespace.as_deref());
    let now = Utc::now();
    let report = run_all(&ctx, &params, now).await;
    Ok(Json(view_report(&report, &now.to_rfc3339())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_ops::doctor::CheckResult;

    /// A context pointed at a closed port: nothing here makes a request, and a
    /// test that accidentally did would fail loudly rather than pass for the
    /// wrong reason.
    fn cluster_wide_ctx() -> OpsCtx {
        OpsCtx {
            client: kube::Client::try_from(kube::Config::new(
                "http://127.0.0.1:1/".parse().expect("a literal URL parses"),
            ))
            .expect("a Config with no auth builds a Client"),
            namespace: "kopiur-system".to_string(),
            scope: Scope::All,
            field_manager: crate::config::FIELD_MANAGER.to_string(),
        }
    }

    /// `?namespace=` moves the SCOPE and nothing else. Moving `ctx.namespace`
    /// too would relocate the `webhook-admits` dry run into the caller's
    /// namespace, where their own missing `create` would read as a broken
    /// webhook.
    #[tokio::test]
    async fn a_doctor_namespace_narrows_the_scope_and_leaves_the_operator_namespace() {
        let scoped = scoped_for(cluster_wide_ctx(), Some("media"));
        assert_eq!(scoped.scope, Scope::Namespace("media".to_string()));
        assert_eq!(
            scoped.namespace, "kopiur-system",
            "the webhook dry run must stay where the operator runs"
        );
    }

    #[tokio::test]
    async fn no_doctor_namespace_stays_cluster_wide() {
        let unscoped = scoped_for(cluster_wide_ctx(), None);
        assert_eq!(
            unscoped.scope,
            Scope::All,
            "absent means the whole installation, never the operator's namespace"
        );
    }

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

    /// The defect this field exists to kill: the UI printed "scoped to `<ns>`"
    /// over `repositories-ready` and `credentials-present`, both of which read
    /// `ClusterRepository` cluster-wide however the run was scoped. A check
    /// that reads a cluster-scoped object must never call itself namespaced —
    /// that is the statement the operator would act on and it would be false.
    #[test]
    fn a_check_that_reads_a_cluster_scoped_object_is_never_namespaced() {
        // Every check whose reads include something `?namespace=` cannot
        // narrow: CRDs and the operator's Deployments are installation state,
        // and the three repository-backed checks all go through `list_repos`,
        // whose `ClusterRepository` listing is `Api::all` unconditionally.
        for check in [
            DoctorCheck::CrdsInstalled,
            DoctorCheck::ControllerRunning,
            DoctorCheck::WebhookRunning,
            DoctorCheck::WebhookAdmits,
            DoctorCheck::RepositoriesReady,
            DoctorCheck::CredentialsPresent,
            DoctorCheck::SnapshotReplications,
        ] {
            assert_ne!(
                check_scope(check),
                DoctorScopeView::Namespace,
                "{} reads a cluster-scoped object, so calling it namespaced tells the \
                 operator a namespace-scoped run covered it when it did not",
                check_id(check)
            );
        }

        // And the converse, so the field cannot be made vacuously safe by
        // answering `mixed` everywhere: the two work-backed checks and the
        // events check read namespaced kinds only, and must say so.
        for check in [
            DoctorCheck::NoStuckWork,
            DoctorCheck::RecentFailures,
            DoctorCheck::RecentWarnings,
        ] {
            assert_eq!(
                check_scope(check),
                DoctorScopeView::Namespace,
                "{} reads only namespaced kinds; reporting it wider hides that \
                 `?namespace=` really did narrow it",
                check_id(check)
            );
        }
    }

    /// The two repository checks are the ones the client's hand-maintained
    /// table got wrong, so pin them by name rather than only by the loop above.
    #[test]
    fn the_repository_checks_report_mixed_because_of_cluster_repositories() {
        assert_eq!(
            check_scope(DoctorCheck::RepositoriesReady),
            DoctorScopeView::Mixed,
            "`list_repos` lists ClusterRepository with `Api::all`, whatever the scope"
        );
        assert_eq!(
            check_scope(DoctorCheck::CredentialsPresent),
            DoctorScopeView::Mixed,
            "it reads the Secrets of those same cluster-scoped repositories"
        );
    }

    /// Every check states a scope, and the wire row carries it. Iterating
    /// `DoctorCheck::ALL` means a check added later is covered here too.
    #[test]
    fn every_check_publishes_its_scope_on_the_wire() {
        for check in DoctorCheck::ALL {
            let row = view_check(check, &Outcome::Pass);
            assert_eq!(
                row.scope,
                check_scope(check),
                "the row must carry the same scope the table states for {}",
                check_id(check)
            );
        }
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
    fn an_absent_checks_selector_runs_everything() {
        assert!(
            parse_checks(None).unwrap().is_none(),
            "absent must mean the full report, not an empty one"
        );
    }

    #[test]
    fn a_checks_selector_is_a_comma_separated_subset() {
        let selected = parse_checks(Some("repositories-ready,no-stuck-work,recent-failures"))
            .unwrap()
            .expect("a named subset is Some");
        assert_eq!(
            selected,
            [
                DoctorCheck::RepositoriesReady,
                DoctorCheck::NoStuckWork,
                DoctorCheck::RecentFailures
            ]
            .into_iter()
            .collect::<BTreeSet<_>>()
        );

        // Whitespace around a token is a copy-paste artefact, not an error.
        assert_eq!(
            parse_checks(Some(" crds-installed , recent-warnings "))
                .unwrap()
                .unwrap(),
            [DoctorCheck::CrdsInstalled, DoctorCheck::RecentWarnings]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
    }

    /// The query is `deny_unknown_fields` so an unknown *parameter* is a 400;
    /// an unknown *value* has to be one too, and it has to say what the valid
    /// ones are — a mistyped id would otherwise silently drop a diagnostic, and
    /// a missing row reads as a passing one.
    #[test]
    fn an_unknown_check_is_a_400_naming_every_valid_id() {
        let err = parse_checks(Some("repositories-ready,no-such-check")).unwrap_err();
        assert_eq!(err.0.status, 400);
        assert!(
            err.0.what.contains("no-such-check"),
            "the refusal must name the offending id, got {}",
            err.0.what
        );
        // Every id, not just a hint — a caller cannot see this list anywhere
        // else, and the list comes from `DoctorCheck::ALL` so it cannot go
        // stale against the parser.
        for check in DoctorCheck::ALL {
            assert!(
                err.0.fix.contains(&check_id(check)),
                "the fix must name `{}`, got {}",
                check_id(check),
                err.0.fix
            );
        }
    }

    /// `?checks=` present but naming nothing would produce a zero-row report,
    /// which looks exactly like a clean bill of health.
    #[test]
    fn an_empty_checks_selector_is_a_400_rather_than_an_empty_report() {
        for raw in ["", " ", ",", " , "] {
            let Err(err) = parse_checks(Some(raw)) else {
                panic!("{raw:?} names no check and must be refused, not run as everything");
            };
            assert_eq!(err.0.status, 400);
            assert!(
                err.0.why.contains("empty report"),
                "the refusal must explain why an empty selector is dangerous, got {}",
                err.0.why
            );
        }
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
