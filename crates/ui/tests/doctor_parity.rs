//! `kubectl kopiur doctor` and the console's Doctor must say the same thing
//! about the same cluster (issue #695).
//!
//! Both surfaces render one [`kopiur_ops::doctor::DoctorReport`], and still
//! disagreed: the CLI scoped its run to the kubeconfig context's namespace
//! while the console ran cluster-wide, so the CLI printed `ok` for failures the
//! console showed. This file is the ratchet that keeps them together:
//!
//! - one fixture report, rendered by the CLI's text renderer and by the
//!   console's wire projection, must yield the same `(check, severity)` rows,
//!   in the same order, as the report itself ([`DoctorReport::verdicts`]);
//! - every check in [`DoctorCheck::ALL`], at every severity, must survive both
//!   renderers — a check one surface drops, or colours differently, fails here;
//! - with no explicit namespace, both surfaces must read the same scope.

use chrono::{DateTime, Utc};
use kopiur_api::{Restore, Snapshot};
use kopiur_ops::doctor::{
    CheckResult, DoctorCheck, DoctorParams, DoctorReport, Outcome, Severity, Work, doctor_scope,
    work_checks,
};
use kopiur_ops::{OpsCtx, Scope};

const NOW: &str = "2026-11-08T16:00:00Z";

fn now() -> DateTime<Utc> {
    NOW.parse().expect("fixed clock")
}

fn ago(minutes: i64) -> String {
    (now() - chrono::Duration::minutes(minutes)).to_rfc3339()
}

/// A terminally Failed Snapshot, decoded the cluster's way (JSON → typed).
fn failed_snapshot(namespace: &str, name: &str, minutes_ago: i64) -> Snapshot {
    serde_json::from_value(serde_json::json!({
        "apiVersion": kopiur_api::consts::API_VERSION,
        "kind": "Snapshot",
        "metadata": { "name": name, "namespace": namespace, "creationTimestamp": ago(minutes_ago + 1) },
        "spec": {},
        "status": {
            "phase": "Failed",
            "conditions": [
                { "type": "Ready", "status": "False", "reason": "MoverJobFailed",
                  "message": "the backup failed (repository connect, RepositoryUnavailable)",
                  "lastTransitionTime": ago(minutes_ago) },
                { "type": "Stalled", "status": "True", "reason": "MoverJobFailed",
                  "message": "the backup failed (repository connect, RepositoryUnavailable)",
                  "lastTransitionTime": ago(minutes_ago) },
            ],
        },
    }))
    .expect("snapshot fixture")
}

fn params() -> DoctorParams {
    DoctorParams {
        stuck_threshold: std::time::Duration::from_secs(3600),
        failure_lookback: std::time::Duration::from_secs(24 * 3600),
        operator_namespace: None,
        checks: None,
    }
}

/// The CLI's rows: `(title, severity)` per check line of the text report.
fn cli_rows(text: &str) -> Vec<(String, Severity)> {
    text.lines()
        // A check line is two spaces and a word; `why:`/`fix:` continuation
        // lines are indented further, the summary is not indented at all.
        .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
        .map(|l| {
            let rest = l.trim_start();
            let (word, title) = rest.split_once(' ').expect("word then title");
            let severity = match word {
                "ok" => Severity::Pass,
                "warn" => Severity::Warn,
                "FAIL" => Severity::Fail,
                other => panic!("the CLI printed an unknown severity word `{other}`: {l}"),
            };
            let title = title.trim_start();
            let title = title.split_once(": ").map_or(title, |(t, _)| t);
            (title.to_string(), severity)
        })
        .collect()
}

/// The console's rows: `(id, severity)` per check of the wire view.
fn ui_rows(report: &DoctorReport) -> Vec<(String, String, Severity)> {
    kopiur_ui::api::doctor::view_report(report, NOW)
        .checks
        .into_iter()
        .map(|c| {
            let severity = match c.outcome.as_str() {
                "Pass" => Severity::Pass,
                "Warn" => Severity::Warn,
                "Fail" => Severity::Fail,
                other => panic!("the console sent an unknown outcome `{other}`"),
            };
            (c.check, c.title, severity)
        })
        .collect()
}

/// Render `report` through both surfaces and assert they agree with the report
/// and with each other, row for row.
fn assert_parity(report: &DoctorReport) {
    let expected = report.verdicts();
    let cli = cli_rows(&kopiur_cli::cmd::doctor::render(report, &Scope::All));
    let ui = ui_rows(report);
    assert_eq!(cli.len(), expected.len(), "CLI rows: {cli:?}");
    assert_eq!(ui.len(), expected.len(), "console rows: {ui:?}");
    for (((id, severity), (cli_title, cli_sev)), (ui_id, ui_title, ui_sev)) in
        expected.iter().zip(&cli).zip(&ui)
    {
        assert_eq!(ui_id, id, "the console names the check by its shared id");
        assert_eq!(cli_title, ui_title, "both surfaces title `{id}` the same");
        assert_eq!(cli_sev, severity, "the CLI's severity for `{id}`");
        assert_eq!(ui_sev, severity, "the console's severity for `{id}`");
    }
}

/// The #695 shape: a recent failure in one namespace, three weeks of retained
/// history in another, a healthy rest — evaluated once, rendered twice.
#[test]
fn one_fixture_report_reads_the_same_on_both_surfaces() {
    let work = Work {
        snapshots: vec![
            failed_snapshot("media", "nightly-1", 30),
            failed_snapshot(
                "id",
                "id-20261020083200-pvc-pocket-id-b6690cd9",
                60 * 24 * 21,
            ),
            failed_snapshot(
                "id",
                "id-20261020093200-pvc-pocket-id-e601b834",
                60 * 24 * 21 - 60,
            ),
        ],
        restores: Vec::<Restore>::new(),
        schedules: Vec::new(),
        policies: Vec::new(),
        degraded: Vec::new(),
    };
    let mut checks: Vec<CheckResult> = [
        DoctorCheck::CrdsInstalled,
        DoctorCheck::ControllerRunning,
        DoctorCheck::WebhookRunning,
        DoctorCheck::WebhookAdmits,
        DoctorCheck::RepositoriesReady,
        DoctorCheck::CredentialsPresent,
        DoctorCheck::SnapshotReplications,
    ]
    .into_iter()
    .map(|c| CheckResult::new(c, Outcome::Pass))
    .collect();
    checks.extend(work_checks(&work, &params(), now()));
    checks.push(CheckResult::new(
        DoctorCheck::RecentWarnings,
        Outcome::Warn("1 warning(s) on kopiur objects in the last hour".into()),
    ));
    let report = DoctorReport { checks };

    // The fixture means what it says: the recent failure fails the check.
    let recent = report
        .verdicts()
        .into_iter()
        .find(|(id, _)| id == "recent-failures")
        .expect("recent-failures ran");
    assert_eq!(recent.1, Severity::Fail);
    assert_parity(&report);

    // History alone warns — on both surfaces.
    let history = Work {
        snapshots: vec![failed_snapshot("id", "old", 60 * 24 * 21)],
        ..work
    };
    let report = DoctorReport {
        checks: work_checks(&history, &params(), now()),
    };
    assert_eq!(
        report.verdicts(),
        vec![
            ("no-stuck-work".to_string(), Severity::Pass),
            ("recent-failures".to_string(), Severity::Warn),
        ]
    );
    assert_parity(&report);
}

/// The ratchet: every check doctor has, at every severity, renders on both
/// surfaces with the same verdict. A check added to the enum is covered with no
/// edit here; a renderer that drops or recolours one fails.
#[test]
fn every_check_at_every_severity_renders_identically_on_both_surfaces() {
    for outcome in [
        Outcome::Pass,
        Outcome::Warn("could not verify".into()),
        Outcome::Fail {
            what: "broken".into(),
            why: "because".into(),
            fix: "fix it".into(),
        },
    ] {
        let report = DoctorReport {
            checks: DoctorCheck::ALL
                .iter()
                .map(|c| CheckResult::new(*c, outcome.clone()))
                .collect(),
        };
        assert_parity(&report);
    }
}

/// With no namespace named, both surfaces read every namespace; with one named,
/// both read exactly it. The CLI used to inherit the kubeconfig namespace here.
#[tokio::test]
async fn both_surfaces_scope_a_run_the_same_way() {
    let ctx = |scope: Scope| OpsCtx {
        client: kube::Client::try_from(kube::Config::new(
            "http://127.0.0.1:1/".parse().expect("a literal URL parses"),
        ))
        .expect("a Config with no auth builds a Client"),
        namespace: "kopiur-system".to_string(),
        scope,
        field_manager: "kopiur-test".to_string(),
    };
    for explicit in [None, Some("media")] {
        // The CLI's context arrives already narrowed to the kubeconfig namespace.
        let cli = kopiur_cli::cmd::doctor::doctor_ctx(
            &ctx(Scope::Namespace("kopiur-system".into())),
            explicit,
        );
        let ui = kopiur_ui::api::doctor::scoped_for(ctx(Scope::All), explicit);
        assert_eq!(cli.scope, ui.scope, "explicit namespace {explicit:?}");
        assert_eq!(cli.scope, doctor_scope(explicit));
    }
    assert_eq!(doctor_scope(None), Scope::All);
}
