//! `kubectl kopiur status` — a one-screen health overview: repositories (with
//! readiness detail), policies (last snapshot / last verify), schedules (last /
//! next fire), and in-flight or stalled work.
//!
//! The cross-CRD join that produces the report lives in `kopiur_ops::status`;
//! what stays here is how this command renders it.

use chrono::{DateTime, Utc};
use kopiur_ops::snapshots::resolve_repo_filter_for;
use kopiur_ops::status::{StatusReport, build_report, gather};

use crate::cli::StatusArgs;
use crate::context::KubeCtx;
use crate::error::CliError;
use crate::output::{EMPTY_CELL, OutputFormat, Table, human_age};

/// Render the report as the human one-screen overview. Pure.
pub fn render(report: &StatusReport, now: DateTime<Utc>) -> String {
    let mut out = String::new();

    out.push_str("REPOSITORIES\n");
    if report.repositories.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let mut t = Table::new(vec![
            "KIND",
            "NAME",
            "NAMESPACE",
            "PHASE",
            "BACKEND",
            "MODE",
            "SUSPENDED",
            "MAINTENANCE",
            "CLUSTER",
            "FOREIGN",
            "DISCOVERED",
        ]);
        for r in &report.repositories {
            t.push(vec![
                r.kind.to_string(),
                r.name.clone(),
                r.namespace.clone().unwrap_or_else(|| EMPTY_CELL.into()),
                r.phase.clone(),
                r.backend.clone(),
                r.mode.clone(),
                r.suspended.to_string(),
                r.maintenance.clone(),
                r.cluster.clone().unwrap_or_else(|| EMPTY_CELL.into()),
                r.foreign_snapshots
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| EMPTY_CELL.into()),
                r.discovered
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| EMPTY_CELL.into()),
            ]);
        }
        out.push_str(&t.render());
        for r in &report.repositories {
            if let Some(problem) = &r.problem {
                out.push_str(&format!("  ! {}/{}: {}\n", r.kind, r.name, problem));
            }
        }
    }

    out.push_str("\nPOLICIES\n");
    if report.policies.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let mut t = Table::new(vec![
            "NAME",
            "NAMESPACE",
            "REPOSITORY",
            "SUSPENDED",
            "LAST-SNAPSHOT",
            "LAST-VERIFIED",
        ]);
        for p in &report.policies {
            t.push(vec![
                p.name.clone(),
                p.namespace.clone(),
                p.repository.clone(),
                p.suspended.to_string(),
                humanize_rfc3339(p.last_snapshot.as_deref(), now),
                humanize_rfc3339(p.last_verified.as_deref(), now),
            ]);
        }
        out.push_str(&t.render());
    }

    out.push_str("\nSCHEDULES\n");
    if report.schedules.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let mut t = Table::new(vec![
            "NAME",
            "NAMESPACE",
            "POLICY",
            "CRON",
            "SUSPENDED",
            "LAST-FIRE",
            "NEXT-FIRE",
            "FAILURES",
        ]);
        for s in &report.schedules {
            t.push(vec![
                s.name.clone(),
                s.namespace.clone(),
                s.policy.clone(),
                s.cron.clone(),
                s.suspended.to_string(),
                humanize_rfc3339(s.last_fire.as_deref(), now),
                s.next_fire.clone().unwrap_or_else(|| EMPTY_CELL.into()),
                s.consecutive_failures
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| EMPTY_CELL.into()),
            ]);
        }
        out.push_str(&t.render());
    }

    // Rendered only when at least one exists: unlike the three core sections,
    // an optional-feature kind most installs never create earns no permanent
    // "(none)" line on the one-screen overview.
    if !report.snapshot_replications.is_empty() {
        out.push_str("\nSNAPSHOT REPLICATIONS\n");
        let mut t = Table::new(vec![
            "NAME",
            "NAMESPACE",
            "SOURCE",
            "DESTINATION",
            "PHASE",
            "SUSPENDED",
            "LAST-REPLICATED",
            "SELECTED",
            "COPIED",
            "PRESENT",
            "FAILED",
            "PRUNED",
        ]);
        for r in &report.snapshot_replications {
            let count = |v: Option<u32>| {
                v.map(|n| n.to_string())
                    .unwrap_or_else(|| EMPTY_CELL.into())
            };
            let run = r.last_run.unwrap_or_default();
            t.push(vec![
                r.name.clone(),
                r.namespace.clone(),
                r.source.clone(),
                r.destination.clone(),
                r.phase.clone(),
                r.suspended.to_string(),
                humanize_rfc3339(r.last_replicated.as_deref(), now),
                count(run.identities_selected),
                count(run.snapshots_copied),
                count(run.already_present),
                count(run.failed),
                count(run.pruned),
            ]);
        }
        out.push_str(&t.render());
        for r in &report.snapshot_replications {
            if let Some(problem) = &r.problem {
                out.push_str(&format!(
                    "  ! SnapshotReplication {}/{}: {}\n",
                    r.namespace, r.name, problem
                ));
            }
        }
    }

    out.push_str(&format!(
        "\nIN FLIGHT: {} snapshot(s), {} restore(s)\n",
        report.in_flight.snapshots, report.in_flight.restores
    ));
    if !report.stalled.is_empty() {
        out.push_str("\nSTALLED (won't progress without intervention):\n");
        for s in &report.stalled {
            out.push_str(&format!("  ! {} {}: {}\n", s.kind, s.object, s.message));
        }
    }
    out
}

/// `2026-06-11T03:00:12Z` → `9h ago`, for the relative columns.
fn humanize_rfc3339(ts: Option<&str>, now: DateTime<Utc>) -> String {
    ts.and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| format!("{} ago", human_age(t.with_timezone(&Utc), now)))
        .unwrap_or_else(|| EMPTY_CELL.into())
}

/// Run `status`: resolve the optional `--repository` filter, read the cluster,
/// join it into the report, then render for the requested format.
pub async fn run(
    ctx: &KubeCtx,
    args: &StatusArgs,
    output: OutputFormat,
    now: DateTime<Utc>,
) -> Result<String, CliError> {
    let repo_filter = match &args.repository {
        None => None,
        Some(name) => Some(
            resolve_repo_filter_for(
                ctx,
                name,
                args.repository_kind.into(),
                args.repository_namespace.as_deref(),
            )
            .await?,
        ),
    };
    let inputs = gather(ctx).await?;
    let report = build_report(&inputs, repo_filter.as_ref());
    match output {
        OutputFormat::Table | OutputFormat::Wide => Ok(render(&report, now)),
        OutputFormat::Yaml => {
            let value = serde_json::to_value(&report).map_err(|e| CliError::Serialization {
                what: "status report",
                source: e.into(),
            })?;
            serde_yaml::to_string(&value).map_err(|e| CliError::Serialization {
                what: "status report",
                source: e.into(),
            })
        }
        OutputFormat::Json => {
            let mut s =
                serde_json::to_string_pretty(&report).map_err(|e| CliError::Serialization {
                    what: "status report",
                    source: e.into(),
                })?;
            s.push('\n');
            Ok(s)
        }
        // There is no single resource to name; the report is the output.
        OutputFormat::Name => Err(CliError::Serialization {
            what: "status report as -o name (status is a report, not a resource; use -o json)",
            source: Box::new(std::io::Error::other("unsupported output format")),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use kopiur_api::SnapshotReplicationRunStats;
    use kopiur_ops::status::{
        InFlight, PolicyRow, RepoRow, ScheduleRow, SnapshotReplicationRow, StalledRow,
    };

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap()
    }

    fn sample() -> StatusReport {
        StatusReport {
            repositories: vec![
                RepoRow {
                    kind: "Repository".to_string(),
                    name: "nas".into(),
                    namespace: Some("media".into()),
                    phase: "Ready".into(),
                    backend: "S3".into(),
                    mode: "ReadWrite".into(),
                    suspended: false,
                    maintenance: "configured".into(),
                    cluster: None,
                    foreign_snapshots: None,
                    discovered: None,
                    problem: None,
                },
                RepoRow {
                    kind: "ClusterRepository".to_string(),
                    name: "offsite".into(),
                    namespace: None,
                    phase: "Failed".into(),
                    backend: "B2".into(),
                    mode: "ReadWrite".into(),
                    suspended: false,
                    maintenance: "-".into(),
                    cluster: Some("east".into()),
                    foreign_snapshots: Some(3),
                    discovered: Some(17),
                    problem: Some("credentials rejected; fix the Secret".into()),
                },
            ],
            policies: vec![PolicyRow {
                name: "nightly".into(),
                namespace: "media".into(),
                repository: "Repository/nas".into(),
                suspended: false,
                last_snapshot: Some("2026-06-11T03:00:12Z".into()),
                last_verified: None,
            }],
            schedules: vec![ScheduleRow {
                name: "nightly".into(),
                namespace: "media".into(),
                policy: "nightly".into(),
                cron: "0 3 * * *".into(),
                suspended: false,
                last_fire: Some("2026-06-11T03:00:00Z".into()),
                next_fire: Some("2026-06-12T03:00:00Z".into()),
                consecutive_failures: Some(2),
            }],
            snapshot_replications: vec![SnapshotReplicationRow {
                name: "to-offsite".into(),
                namespace: "media".into(),
                source: "Repository/nas".into(),
                destination: "ClusterRepository/offsite".into(),
                phase: "Succeeded".into(),
                suspended: false,
                last_replicated: Some("2026-06-11T06:00:00Z".into()),
                last_run: Some(SnapshotReplicationRunStats {
                    identities_selected: Some(4),
                    snapshots_copied: Some(12),
                    already_present: Some(88),
                    failed: Some(0),
                    pruned: Some(2),
                }),
                problem: Some("destination repository not Ready".into()),
            }],
            in_flight: InFlight {
                snapshots: 1,
                restores: 0,
            },
            stalled: vec![StalledRow {
                kind: "Snapshot".to_string(),
                object: "media/oops".into(),
                message: "terminal kopia failure".into(),
            }],
        }
    }

    #[test]
    fn render_shows_sections_problems_and_stalled() {
        let text = render(&sample(), now());
        assert!(text.contains("REPOSITORIES"), "{text}");
        assert!(text.contains("Repository         nas"), "{text}");
        // Multi-cluster columns: unset shows the empty-cell dash, set shows the
        // cluster name and foreign-snapshot count.
        assert!(text.contains("CLUSTER"), "{text}");
        assert!(text.contains("FOREIGN"), "{text}");
        assert!(
            text.lines().any(|l| l.starts_with("ClusterRepository")
                && l.contains("east")
                && l.contains('3')),
            "{text}"
        );
        // A non-Ready repo carries its Ready-condition message inline.
        assert!(
            text.contains("! ClusterRepository/offsite: credentials rejected"),
            "{text}"
        );
        assert!(text.contains("LAST-SNAPSHOT"), "{text}");
        assert!(text.contains("9h ago"), "{text}");
        assert!(
            text.contains("IN FLIGHT: 1 snapshot(s), 0 restore(s)"),
            "{text}"
        );
        assert!(text.contains("STALLED"), "{text}");
        assert!(
            text.contains("! Snapshot media/oops: terminal kopia failure"),
            "{text}"
        );
        // consecutiveFailures surfaces in the FAILURES column.
        assert!(
            text.lines()
                .any(|l| l.contains("0 3 * * *") && l.ends_with('2')),
            "{text}"
        );
    }

    #[test]
    fn the_discovered_column_shows_the_catalog_count_or_the_empty_cell() {
        let text = render(&sample(), now());
        assert!(text.contains("DISCOVERED"), "{text}");
        // A scanned repository shows its count — plain inventory, no judgment.
        assert!(
            text.lines()
                .any(|l| l.starts_with("ClusterRepository") && l.ends_with("17")),
            "{text}"
        );
        // A never-scanned repository shows the empty-cell dash (the row's
        // trailing columns are all unset, and Table strips trailing padding).
        assert!(
            text.lines()
                .any(|l| l.starts_with("Repository ") && l.ends_with(EMPTY_CELL)),
            "{text}"
        );
    }

    #[test]
    fn render_shows_the_snapshot_replication_section_with_last_run_counts() {
        let text = render(&sample(), now());
        assert!(text.contains("SNAPSHOT REPLICATIONS"), "{text}");
        // source→dest, phase, lastReplicated, and the five lastRun counters.
        assert!(
            text.lines().any(|l| l.starts_with("to-offsite")
                && l.contains("Repository/nas")
                && l.contains("ClusterRepository/offsite")
                && l.contains("Succeeded")
                && l.contains("6h ago")),
            "{text}"
        );
        for header in ["SELECTED", "COPIED", "PRESENT", "FAILED", "PRUNED"] {
            assert!(text.contains(header), "missing {header}: {text}");
        }
        assert!(
            text.lines()
                .any(|l| l.starts_with("to-offsite") && l.contains("88")),
            "alreadyPresent count must render: {text}"
        );
        // The Ready-condition problem line renders like the repository ones.
        assert!(
            text.contains(
                "! SnapshotReplication media/to-offsite: destination repository not Ready"
            ),
            "{text}"
        );
    }

    #[test]
    fn empty_snapshot_replications_render_no_section() {
        let mut report = sample();
        report.snapshot_replications.clear();
        let text = render(&report, now());
        assert!(!text.contains("SNAPSHOT REPLICATIONS"), "{text}");
    }

    #[test]
    fn snapshot_replication_row_serializes_last_run_camel_case() {
        let v = serde_json::to_value(sample()).unwrap();
        let row = &v["snapshotReplications"][0];
        assert_eq!(row["lastReplicated"], "2026-06-11T06:00:00Z");
        assert_eq!(row["lastRun"]["identitiesSelected"], 4);
        assert_eq!(row["lastRun"]["snapshotsCopied"], 12);
        assert_eq!(row["lastRun"]["alreadyPresent"], 88);
        assert_eq!(row["lastRun"]["failed"], 0);
        assert_eq!(row["lastRun"]["pruned"], 2);
    }

    #[test]
    fn report_serializes_camel_case_for_machine_output() {
        let v = serde_json::to_value(sample()).unwrap();
        assert_eq!(v["inFlight"]["snapshots"], 1);
        assert_eq!(v["policies"][0]["lastSnapshot"], "2026-06-11T03:00:12Z");
        assert_eq!(
            v["repositories"][1]["problem"],
            "credentials rejected; fix the Secret"
        );
        assert_eq!(v["repositories"][1]["cluster"], "east");
        assert_eq!(v["repositories"][1]["foreignSnapshots"], 3);
        assert_eq!(v["repositories"][1]["discovered"], 17);
        // Unset multi-cluster / catalog fields are elided, not `null`.
        assert!(v["repositories"][0].get("cluster").is_none());
        assert!(v["repositories"][0].get("foreignSnapshots").is_none());
        assert!(v["repositories"][0].get("discovered").is_none());
    }
}
