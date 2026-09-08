//! `kubectl kopiur snapshots list` — a richer `kubectl get snapshots`: policy,
//! origin, size, and file counts in one view, filterable by policy/origin/
//! repository, across namespaces with `-A`.
//!
//! The selection half (filters, matchers, ordering, the list call) lives in
//! `kopiur_ops::snapshots`; what stays here is the table this command draws.

use chrono::{DateTime, Utc};
use kopiur_api::common::PhaseLabel;
use kopiur_api::consts::CONFIG_LABEL;
use kopiur_api::{Origin, Snapshot, SnapshotPhase};
use kopiur_ops::OpsError;
use kopiur_ops::snapshots::{
    RepoFilter, SnapshotListFilter, label_selector, list_snapshots, matches_repository, meta_time,
    resolve_repo_filter_for,
};
use kube::ResourceExt;

use crate::cli::SnapshotsListArgs;
use crate::context::{KubeCtx, Scope};
use crate::error::CliError;
use crate::output::{EMPTY_CELL, OutputFormat, Table, human_age, human_bytes};

/// The table headers, in render order. `namespaced` adds the NAMESPACE column
/// (for `-A`), `wide` appends the detail columns.
pub fn headers(all_namespaces: bool, wide: bool) -> Vec<&'static str> {
    let mut h = vec!["NAME"];
    if all_namespaces {
        h.push("NAMESPACE");
    }
    h.extend([
        "POLICY",
        "ORIGIN",
        "PHASE",
        "SNAPSHOT-ID",
        "SIZE",
        "FILES",
        "START",
        "AGE",
    ]);
    if wide {
        h.extend([
            "IDENTITY",
            "DELETION-POLICY",
            "PINNED",
            "RECORDED",
            "DESCRIPTION",
            "COPIED-FROM",
        ]);
    }
    h
}

/// Render `status.recorded` as a compact `uid:gid` cell (`-` per missing part;
/// the whole cell empty when nothing was recorded). The recorded identity is
/// what a Phase-3 restore will reproduce, so surfacing it here is what makes
/// the feature visible from the plugin at all.
fn recorded_cell(status: Option<&kopiur_api::SnapshotStatus>) -> String {
    match status.and_then(|s| s.recorded.as_ref()) {
        None => EMPTY_CELL.into(),
        Some(rec) => {
            let part = |v: Option<i64>| v.map_or_else(|| EMPTY_CELL.into(), |n| n.to_string());
            format!("{}:{}", part(rec.uid), part(rec.gid))
        }
    }
}

/// The kopia description, truncated for the table (full value via `-o yaml`).
fn description_cell(status: Option<&kopiur_api::SnapshotStatus>) -> String {
    const MAX: usize = 40;
    match status
        .and_then(|s| s.snapshot.as_ref())
        .and_then(|i| i.description.as_deref())
    {
        None => EMPTY_CELL.into(),
        Some(d) if d.chars().count() <= MAX => d.to_string(),
        Some(d) => {
            let cut: String = d.chars().take(MAX - 1).collect();
            format!("{cut}…")
        }
    }
}

/// `status.copiedFrom` as a compact `<Kind>/<name>:<sourceManifestId>` cell —
/// the replication lineage of an `origin: replicated` row (which SOURCE
/// repository it was migrated from and which manifest it corresponds to
/// there). The named CLI consumer of `Snapshot.status.copiedFrom`; empty on
/// every other origin. Full detail (including the preserved `startTime`) stays
/// available via `-o yaml`.
fn copied_from_cell(status: Option<&kopiur_api::SnapshotStatus>) -> String {
    match status.and_then(|s| s.copied_from.as_ref()) {
        None => EMPTY_CELL.into(),
        Some(cf) => format!(
            "{:?}/{}:{}",
            cf.repository.kind, cf.repository.name, cf.source_manifest_id
        ),
    }
}

fn origin_cell(origin: Option<Origin>) -> String {
    match origin {
        Some(Origin::Scheduled) => "scheduled".into(),
        Some(Origin::Manual) => "manual".into(),
        Some(Origin::Discovered) => "discovered".into(),
        Some(Origin::Adopted) => "adopted".into(),
        Some(Origin::Replicated) => "replicated".into(),
        None => EMPTY_CELL.into(),
    }
}

fn phase_cell(phase: Option<&SnapshotPhase>) -> String {
    phase.map_or_else(|| EMPTY_CELL.into(), |p| p.label().to_string())
}

/// One table row for a Snapshot. Pure; `now` is injected for a deterministic AGE.
pub fn row(snap: &Snapshot, now: DateTime<Utc>, all_namespaces: bool, wide: bool) -> Vec<String> {
    let status = snap.status.as_ref();
    let policy = snap
        .spec
        .policy_ref
        .as_ref()
        .map(|p| p.name.clone())
        .or_else(|| {
            snap.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(CONFIG_LABEL).cloned())
        })
        .unwrap_or_else(|| EMPTY_CELL.into());
    let stats = status.and_then(|s| s.stats.as_ref());
    let size = stats
        .and_then(|s| s.size_bytes)
        .map_or_else(|| EMPTY_CELL.into(), human_bytes);
    let files = stats
        .map(|s| [s.files_new, s.files_modified, s.files_unchanged])
        .filter(|counts| counts.iter().any(Option::is_some))
        .map(|counts| counts.into_iter().flatten().sum::<i64>().to_string())
        .unwrap_or_else(|| EMPTY_CELL.into());
    let start = status
        .and_then(|s| s.timing.as_ref())
        .and_then(|t| t.start_time.clone())
        .unwrap_or_else(|| EMPTY_CELL.into());
    let age = snap
        .metadata
        .creation_timestamp
        .as_ref()
        .and_then(meta_time)
        .map_or_else(|| EMPTY_CELL.into(), |t| human_age(t, now));

    let mut cells = vec![snap.name_any()];
    if all_namespaces {
        cells.push(
            snap.metadata
                .namespace
                .clone()
                .unwrap_or_else(|| EMPTY_CELL.into()),
        );
    }
    cells.extend([
        policy,
        origin_cell(status.and_then(|s| s.origin)),
        phase_cell(status.and_then(|s| s.phase.as_ref())),
        status
            .and_then(|s| s.snapshot.as_ref())
            .map(|i| i.kopia_snapshot_id.clone())
            .unwrap_or_else(|| EMPTY_CELL.into()),
        size,
        files,
        start,
        age,
    ]);
    if wide {
        let identity = status
            .and_then(|s| s.snapshot.as_ref())
            .map(|i| match &i.identity.source_path {
                Some(path) => format!("{}@{}:{}", i.identity.username, i.identity.hostname, path),
                None => format!("{}@{}", i.identity.username, i.identity.hostname),
            })
            .unwrap_or_else(|| EMPTY_CELL.into());
        let deletion = snap
            .spec
            .deletion_policy
            .map(|d| {
                match d {
                    kopiur_api::DeletionPolicy::Delete => "delete",
                    kopiur_api::DeletionPolicy::Retain => "retain",
                    kopiur_api::DeletionPolicy::Orphan => "orphan",
                }
                .to_string()
            })
            .unwrap_or_else(|| EMPTY_CELL.into());
        let pinned = status
            .and_then(|s| s.pinned)
            .map_or_else(|| EMPTY_CELL.into(), |p| p.to_string());
        cells.extend([
            identity,
            deletion,
            pinned,
            recorded_cell(status),
            description_cell(status),
            copied_from_cell(status),
        ]);
    }
    cells
}

/// Resolve the `snapshots list` flags into an optional [`RepoFilter`].
async fn resolve_repo_filter(
    ctx: &KubeCtx,
    args: &SnapshotsListArgs,
) -> Result<Option<RepoFilter>, CliError> {
    let Some(name) = &args.repository else {
        return Ok(None);
    };
    let filter = resolve_repo_filter_for(
        ctx,
        name,
        args.repository_kind.into(),
        args.repository_namespace.as_deref(),
    )
    .await?;
    Ok(Some(filter))
}

/// Run `snapshots list` and render for the requested format.
pub async fn list(
    ctx: &KubeCtx,
    args: &SnapshotsListArgs,
    output: OutputFormat,
    now: DateTime<Utc>,
) -> Result<String, CliError> {
    let repo_filter = resolve_repo_filter(ctx, args).await?;

    let filter = SnapshotListFilter {
        policy: args.policy.clone(),
        origin: args.origin.map(Origin::from),
    };
    let selector = label_selector(&filter);
    let snaps: Vec<Snapshot> = list_snapshots(ctx, selector.as_deref())
        .await?
        .into_iter()
        .filter(|s| {
            repo_filter
                .as_ref()
                .is_none_or(|f| matches_repository(s, f))
        })
        .collect();

    render_list(&snaps, &ctx.scope, output, now)
}

/// Render the filtered, sorted list. Pure.
pub fn render_list(
    snaps: &[Snapshot],
    scope: &Scope,
    output: OutputFormat,
    now: DateTime<Utc>,
) -> Result<String, CliError> {
    let all_namespaces = matches!(scope, Scope::All);
    match output {
        OutputFormat::Table | OutputFormat::Wide => {
            if snaps.is_empty() {
                return Ok(match scope {
                    Scope::All => "No snapshots found.\n".to_string(),
                    Scope::Namespace(ns) => format!("No snapshots found in namespace {ns}.\n"),
                });
            }
            let wide = matches!(output, OutputFormat::Wide);
            let mut table = Table::new(headers(all_namespaces, wide));
            for snap in snaps {
                table.push(row(snap, now, all_namespaces, wide));
            }
            Ok(table.render())
        }
        OutputFormat::Yaml | OutputFormat::Json => {
            let list = serde_json::json!({
                "apiVersion": "v1",
                "kind": "List",
                "items": snaps,
            });
            match output {
                OutputFormat::Yaml => serde_yaml::to_string(&list).map_err(|e| {
                    CliError::Ops(OpsError::Serialization {
                        what: "snapshot list",
                        source: e.into(),
                    })
                }),
                _ => {
                    let mut s = serde_json::to_string_pretty(&list).map_err(|e| {
                        CliError::Ops(OpsError::Serialization {
                            what: "snapshot list",
                            source: e.into(),
                        })
                    })?;
                    s.push('\n');
                    Ok(s)
                }
            }
        }
        OutputFormat::Name => Ok(snaps
            .iter()
            .map(|s| format!("snapshot.{}/{}\n", kopiur_api::GROUP, s.name_any()))
            .collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Parse a manifest the way the cluster does (YAML → JSON value → typed),
    /// matching the api crate's testutil convention.
    fn from_yaml<T: serde::de::DeserializeOwned>(yaml: &str) -> T {
        let value: serde_json::Value = serde_yaml::from_str(yaml).expect("yaml -> json value");
        serde_json::from_value(value).expect("json value -> typed")
    }

    const SUCCEEDED_SNAPSHOT: &str = r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: nightly-20260611
  namespace: media
  creationTimestamp: "2026-06-11T03:00:00Z"
  labels:
    kopiur.home-operations.com/origin: scheduled
    kopiur.home-operations.com/config: nightly
spec:
  policyRef:
    name: nightly
  deletionPolicy: Delete
status:
  phase: Succeeded
  origin: scheduled
  snapshot:
    kopiaSnapshotID: a1b2c3d4e5f6
    identity:
      username: nightly
      hostname: media
      sourcePath: /pvc/data
  timing:
    startTime: "2026-06-11T03:00:12Z"
    endTime: "2026-06-11T03:05:12Z"
    durationSeconds: 300
  stats:
    sizeBytes: 5368709120
    filesNew: 10
    filesModified: 5
    filesUnchanged: 985
  resolved:
    repository:
      kind: Repository
      name: nas
"#;

    #[test]
    fn row_renders_the_succeeded_snapshot() {
        let snap: Snapshot = from_yaml(SUCCEEDED_SNAPSHOT);
        let now = Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap();
        let cells = row(&snap, now, false, false);
        assert_eq!(
            cells,
            vec![
                "nightly-20260611",
                "nightly",
                "scheduled",
                "Succeeded",
                "a1b2c3d4e5f6",
                "5.0 GiB",
                "1000",
                "2026-06-11T03:00:12Z",
                "9h",
            ]
        );
    }

    #[test]
    fn wide_row_appends_identity_deletion_policy_pin_recorded_and_description() {
        let snap: Snapshot = from_yaml(SUCCEEDED_SNAPSHOT);
        let now = Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap();
        let cells = row(&snap, now, true, true);
        // -A inserts NAMESPACE after NAME.
        assert_eq!(cells[1], "media");
        let tail = &cells[cells.len() - 6..];
        // Nothing recorded / no description / not replicated → placeholder cells.
        assert_eq!(
            tail,
            ["nightly@media:/pvc/data", "delete", "-", "-", "-", "-"]
        );
        assert_eq!(headers(true, true).len(), cells.len());
    }

    #[test]
    fn wide_row_renders_recorded_identity_and_description() {
        let mut snap: Snapshot = from_yaml(SUCCEEDED_SNAPSHOT);
        let status = snap.status.as_mut().unwrap();
        status.recorded = serde_json::from_value(serde_json::json!({
            "schema": 1, "src": "explicit", "uid": 3001, "fsGroup": 65532
        }))
        .unwrap();
        status.snapshot.as_mut().unwrap().description =
            Some("pre-upgrade snapshot with a description far past the forty character cap".into());
        let now = Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap();
        let cells = row(&snap, now, false, true);
        let tail = &cells[cells.len() - 3..];
        // uid recorded, gid image-determined → `3001:-`; description truncated
        // with an ellipsis (the full value stays available via -o yaml).
        assert_eq!(tail[0], "3001:-");
        assert!(tail[1].starts_with("pre-upgrade snapshot"), "{}", tail[1]);
        assert!(tail[1].ends_with('…'), "{}", tail[1]);
        assert!(tail[1].chars().count() <= 40);
        // Not a replicated row → COPIED-FROM stays the placeholder.
        assert_eq!(tail[2], "-");
    }

    #[test]
    fn wide_row_renders_copied_from_for_a_replicated_row() {
        // The named consumer of Snapshot.status.copiedFrom: a replicated copy
        // CR shows which source repository (and source manifest) it came from.
        let snap: Snapshot = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: srepl-copy-1
  namespace: media
  creationTimestamp: "2026-06-11T06:00:00Z"
  labels:
    kopiur.home-operations.com/origin: replicated
spec:
  deletionPolicy: Delete
status:
  phase: Succeeded
  origin: replicated
  snapshot:
    kopiaSnapshotID: dst111
    identity:
      username: pg
      hostname: billing
      sourcePath: /pvc/data
  copiedFrom:
    repository:
      kind: Repository
      name: nas-src
    sourceManifestId: src999
    startTime: "2026-06-10T03:00:00Z"
"#,
        );
        let now = Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap();
        let cells = row(&snap, now, false, true);
        assert_eq!(cells.last().unwrap(), "Repository/nas-src:src999");
        // Narrow (non-wide) rows still render the origin column as replicated.
        let narrow = row(&snap, now, false, false);
        assert!(narrow.contains(&"replicated".to_string()), "{narrow:?}");
    }

    #[test]
    fn discovered_snapshot_with_empty_spec_renders_placeholders() {
        let snap: Snapshot = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: discovered-1
  namespace: media
  labels:
    kopiur.home-operations.com/origin: discovered
    kopiur.home-operations.com/repository-uid: repo-uid-1
spec: {}
status:
  phase: Discovered
  origin: discovered
"#,
        );
        let now = Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap();
        let cells = row(&snap, now, false, false);
        assert_eq!(
            cells,
            vec![
                "discovered-1",
                "-",
                "discovered",
                "Discovered",
                "-",
                "-",
                "-",
                "-",
                "-"
            ]
        );
    }

    #[test]
    fn empty_table_says_no_snapshots_with_scope() {
        let now = Utc::now();
        let out = render_list(
            &[],
            &Scope::Namespace("media".into()),
            OutputFormat::Table,
            now,
        )
        .unwrap();
        assert_eq!(out, "No snapshots found in namespace media.\n");
        let out = render_list(&[], &Scope::All, OutputFormat::Table, now).unwrap();
        assert_eq!(out, "No snapshots found.\n");
    }

    #[test]
    fn name_output_matches_kubectl_o_name() {
        let snap: Snapshot = from_yaml(SUCCEEDED_SNAPSHOT);
        let now = Utc::now();
        let out = render_list(
            &[snap],
            &Scope::Namespace("media".into()),
            OutputFormat::Name,
            now,
        )
        .unwrap();
        assert_eq!(
            out,
            "snapshot.kopiur.home-operations.com/nightly-20260611\n"
        );
    }

    #[test]
    fn json_output_is_a_v1_list_of_verbatim_objects() {
        let snap: Snapshot = from_yaml(SUCCEEDED_SNAPSHOT);
        let now = Utc::now();
        let out = render_list(&[snap], &Scope::All, OutputFormat::Json, now).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["kind"], "List");
        assert_eq!(v["items"][0]["kind"], "Snapshot");
        assert_eq!(
            v["items"][0]["status"]["snapshot"]["kopiaSnapshotID"],
            "a1b2c3d4e5f6"
        );
    }
}
