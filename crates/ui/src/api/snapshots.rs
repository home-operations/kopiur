//! The `Snapshot` endpoints: the filtered, paginated list and per-snapshot
//! detail (stats, lineage, retention preview, gates, log tail).
//!
//! # The list is capped, never truncated
//!
//! A cluster can hold tens of thousands of `Snapshot` CRs. Assembling all of
//! them into one JSON body would take the server and the browser down together,
//! and quietly returning the first N would be worse: a backup table that silently
//! dropped rows reads as "these are all my backups". So a filter that selects
//! more than `KOPIUR_UI_SNAPSHOT_LIST_CAP` rows is a 422 naming the filters that
//! would narrow it.
//!
//! # Redaction is at the edge, not in the middle
//!
//! `status.logTail` and `status.failure.message` are kopia's own output, which
//! can carry a presigned URL or a token echoed back in an error. Both go through
//! [`crate::auth::redact::redact_text`] on the way out — the last thing that
//! happens before they become JSON.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::{Json, Router, routing::get};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use kopiur_api::common::RepositoryKind;
use kopiur_api::consts::CONFIG_LABEL;
use kopiur_api::gates::GateScope;
use kopiur_api::retention::{SnapshotLike, select_kept};
use kopiur_api::snapshot::repository_ref_for;
use kopiur_api::{Origin, Snapshot, SnapshotPhase, SnapshotPolicy};
use kopiur_ops::snapshots::{
    RepoFilter, SnapshotListFilter, matches_filter, matches_repository, policy_of,
    resolve_repo_filter_for, sort_key,
};
use kopiur_ui_model::views::{
    FailureView, Lineage, Page, RetentionPreview, SnapshotDetail, SnapshotRefView, SnapshotRow,
    SnapshotStatsView,
};

use crate::AppState;
use crate::api::problem::{ApiError, problem};
use crate::api::{
    RepositoryKindPath, client_for, conditions_view, gate_hits, list_too_large, ops_ctx,
    origin_view, paginate, repo_ref_display, snapshot_phase_view,
};
use crate::auth::CurrentIdentity;
use crate::auth::identity::Identity;
use crate::auth::redact::redact_text;

/// How many rows a page holds when the caller does not say.
const DEFAULT_LIMIT: usize = 50;
/// The largest page a caller may ask for, however large a `limit` they send.
const MAX_LIMIT: usize = 500;

/// This module's routes, relative to `/api/v1`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/snapshots", get(list))
        .route("/snapshots/{namespace}/{name}", get(detail))
}

/// The snapshot table's query string.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotQuery {
    /// Only snapshots in this repository.
    #[serde(default)]
    pub repository: Option<String>,
    /// Which repository CRD `repository` names; defaults to `repository`.
    #[serde(default)]
    pub repository_kind: Option<RepositoryKindPath>,
    /// The namespace the named `Repository` lives in; ignored for a
    /// `ClusterRepository`.
    #[serde(default)]
    pub repository_namespace: Option<String>,
    /// Only snapshots produced from this `SnapshotPolicy`.
    #[serde(default)]
    pub policy: Option<String>,
    /// Only snapshots with this origin, as the wire value
    /// (`scheduled`/`manual`/`discovered`/`adopted`/`replicated`).
    #[serde(default)]
    pub origin: Option<String>,
    /// Only snapshots in this phase, as the wire view name
    /// (`succeeded`, `failed`, …) plus `unknown` for anything this build does
    /// not recognize.
    #[serde(default)]
    pub phase: Option<String>,
    /// Restrict to this namespace; absent lists cluster-wide.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Index of the first row to return.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum rows to return; clamped to [`MAX_LIMIT`].
    #[serde(default)]
    pub limit: Option<usize>,
}

/// The filters [`filter_rows`] applies, already parsed.
///
/// Separate from [`SnapshotQuery`] because parsing can fail with a 400 and
/// filtering cannot — once this exists, every remaining decision is total.
///
/// # Why the label filters are applied client-side
///
/// `labels` is exactly the `kopiur_ops::SnapshotListFilter` the CLI builds from
/// `--policy`/`--origin`, and the CLI hands it to the apiserver as a label
/// selector. This API cannot: reads go through [`crate::cache::Source`], whose
/// cache arm answers from reflector stores that hold whole kinds and have no
/// selector to push anywhere. So the same filter is evaluated here instead, by
/// [`matches_filter`] — the client-side twin of `label_selector`, which reads
/// the same two labels, with a test in `kopiur_ops` pinning the two against each
/// other. The alternative, re-deriving "is this the right policy?" locally, is
/// what let this endpoint filter origin on `status.origin` while the CLI
/// filtered it on the label.
#[derive(Debug, Clone, Default)]
pub struct ParsedFilter {
    /// The resolved repository, when one was named. Not a label filter: a
    /// produced snapshot records its repository in status, not in a label.
    pub repository: Option<RepoFilter>,
    /// The policy and origin, in the shared ops type.
    pub labels: SnapshotListFilter,
    /// The phase to match. Not a label filter either — the phase lives in
    /// status, so the CLI cannot select on it server-side and neither can this.
    pub phase: Option<PhaseFilter>,
}

/// A phase the caller asked for.
///
/// `Unknown` is its own variant rather than a `SnapshotPhase::Unknown(String)`
/// because the caller is asking "show me everything this build cannot interpret",
/// not "show me the phase literally spelled `unknown`".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseFilter {
    /// One canonical phase.
    Exactly(SnapshotPhase),
    /// Any phase this build does not recognize.
    Unrecognized,
}

/// **Pure.** Parse a `?phase=` value.
///
/// The accepted values are the wire view's own camelCase names, so the string
/// the SPA received in a row is the string it can send back as a filter.
/// Exhaustive over [`SnapshotPhase`], so a new phase becomes filterable the
/// moment it is declared rather than silently matching nothing.
pub fn parse_phase(value: &str) -> Option<PhaseFilter> {
    if value == "unknown" {
        return Some(PhaseFilter::Unrecognized);
    }
    let phase = match value {
        "pending" => SnapshotPhase::Pending,
        "running" => SnapshotPhase::Running,
        "succeeded" => SnapshotPhase::Succeeded,
        "failed" => SnapshotPhase::Failed,
        "deleting" => SnapshotPhase::Deleting,
        "discovered" => SnapshotPhase::Discovered,
        "unchanged" => SnapshotPhase::Unchanged,
        _other => return None,
    };
    Some(PhaseFilter::Exactly(phase))
}

/// **Pure.** Whether a snapshot's phase satisfies the filter. Exhaustive over
/// both the filter and the phase.
fn phase_matches(snap: &Snapshot, filter: &PhaseFilter) -> bool {
    let Some(actual) = snap.status.as_ref().and_then(|s| s.phase.as_ref()) else {
        return false;
    };
    match filter {
        PhaseFilter::Exactly(wanted) => actual == wanted,
        PhaseFilter::Unrecognized => match actual {
            SnapshotPhase::Unknown(_) => true,
            SnapshotPhase::Pending
            | SnapshotPhase::Running
            | SnapshotPhase::Succeeded
            | SnapshotPhase::Failed
            | SnapshotPhase::Deleting
            | SnapshotPhase::Discovered
            | SnapshotPhase::Unchanged => false,
        },
    }
}

/// **Pure.** Apply every parsed filter, newest run first.
pub fn filter_rows(snapshots: &[Arc<Snapshot>], filter: &ParsedFilter) -> Vec<Arc<Snapshot>> {
    let mut kept: Vec<Arc<Snapshot>> = snapshots
        .iter()
        .filter(|s| {
            filter
                .repository
                .as_ref()
                .is_none_or(|r| matches_repository(s, r))
        })
        .filter(|s| matches_filter(s, &filter.labels))
        .filter(|s| filter.phase.as_ref().is_none_or(|p| phase_matches(s, p)))
        .cloned()
        .collect();
    // Newest run first, with the CR name as the tiebreaker so two snapshots
    // that started in the same second do not swap places between polls.
    kept.sort_by(|a, b| {
        sort_key(b)
            .cmp(&sort_key(a))
            .then_with(|| a.metadata.name.cmp(&b.metadata.name))
    });
    kept
}

/// **Pure.** One snapshot as a table row.
pub fn view_row(snap: &Snapshot) -> SnapshotRow {
    let status = snap.status.as_ref();
    let info = status.and_then(|s| s.snapshot.as_ref());
    let timing = status.and_then(|s| s.timing.as_ref());
    let stats = status.and_then(|s| s.stats.as_ref());
    let namespace = snap.metadata.namespace.clone().unwrap_or_default();
    SnapshotRow {
        name: snap.metadata.name.clone().unwrap_or_default(),
        phase: status
            .and_then(|s| s.phase.as_ref())
            .map(snapshot_phase_view),
        origin: status.and_then(|s| s.origin).map(origin_view),
        policy: policy_of(snap).map(str::to_string),
        repository: repository_ref_for(snap).map(|r| repo_ref_display(&r, Some(&namespace))),
        kopia_snapshot_id: info.map(|i| i.kopia_snapshot_id.clone()),
        identity: info.map(|i| kopiur_api::identity::identity_string(&i.identity)),
        start_time: timing.and_then(|t| t.start_time.clone()),
        end_time: timing.and_then(|t| t.end_time.clone()),
        size_bytes: stats.and_then(|s| s.size_bytes),
        bytes_new: stats.and_then(|s| s.bytes_new),
        files_total: stats.and_then(files_total),
        files_failed: stats.and_then(|s| s.files_failed),
        // The observed kopia-side pin wins over the request: `spec.pin` is what
        // was asked for, `status.pinned` is what the repository actually holds.
        pinned: status.and_then(|s| s.pinned).unwrap_or(snap.spec.pin),
        deletion_policy: snap.spec.deletion_policy.map(|p| format!("{p:?}")),
        copied_from: status
            .and_then(|s| s.copied_from.as_ref())
            .map(|c| repo_ref_display(&c.repository, Some(&namespace))),
        namespace,
    }
}

/// **Pure.** How many files the run considered, summed from kopia's per-class
/// counters. `None` when the run recorded none of them, so an unreported count
/// never renders as a confident zero.
fn files_total(stats: &kopiur_api::snapshot::SnapshotStats) -> Option<i64> {
    let counters = [stats.files_new, stats.files_modified, stats.files_unchanged];
    if counters.iter().all(Option::is_none) {
        return None;
    }
    Some(counters.iter().map(|c| c.unwrap_or(0)).sum())
}

/// **Pure.** kopia's upload counters for one run.
fn stats_view(stats: &kopiur_api::snapshot::SnapshotStats) -> SnapshotStatsView {
    SnapshotStatsView {
        size_bytes: stats.size_bytes,
        bytes_new: stats.bytes_new,
        files_new: stats.files_new,
        files_modified: stats.files_modified,
        files_unchanged: stats.files_unchanged,
        files_failed: stats.files_failed,
    }
}

/// **Pure.** Structured failure detail, with kopia's own text redacted.
fn failure_view(f: &kopiur_api::common::FailureBlock) -> FailureView {
    FailureView {
        kopia_error_class: Some(f.kopia_error_class.clone()),
        message: Some(redact_text(&f.message)),
        exit_code: f.exit_code,
        retry_recommended: Some(f.retry_recommended),
        op: f.op.clone(),
    }
}

/// **Pure.** Where this snapshot came from and which snapshots are copies of it.
///
/// A copy records the *source* manifest id, which the destination's own id never
/// equals — `snapshot migrate` mints a new one — so the correlation is
/// `copiedFrom.sourceManifestId == this.kopiaSnapshotID`, one direction only.
pub fn view_lineage(snap: &Snapshot, siblings: &[Arc<Snapshot>]) -> Lineage {
    let status = snap.status.as_ref();
    let namespace = snap.metadata.namespace.clone().unwrap_or_default();
    let own_id = status
        .and_then(|s| s.snapshot.as_ref())
        .map(|i| i.kopia_snapshot_id.as_str());
    let copies = own_id
        .map(|id| {
            siblings
                .iter()
                .filter(|other| {
                    other
                        .status
                        .as_ref()
                        .and_then(|s| s.copied_from.as_ref())
                        .is_some_and(|c| c.source_manifest_id == id)
                })
                .map(|other| SnapshotRefView {
                    namespace: other.metadata.namespace.clone().unwrap_or_default(),
                    name: other.metadata.name.clone().unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    let copied_from = status.and_then(|s| s.copied_from.as_ref());
    Lineage {
        copied_from_repository: copied_from
            .map(|c| repo_ref_display(&c.repository, Some(&namespace))),
        source_manifest_id: copied_from.map(|c| c.source_manifest_id.clone()),
        copies,
    }
}

/// A `Snapshot` seen through the retention selector's eyes.
///
/// The adapter exists so the preview answers with *exactly* the rule the
/// operator's own prune would apply — same `select_kept`, same inputs — rather
/// than with a second implementation of GFS that could disagree with it.
struct RetentionCandidate {
    id: String,
    end: DateTime<Utc>,
    pinned: bool,
}

impl SnapshotLike for RetentionCandidate {
    fn end_time(&self) -> DateTime<Utc> {
        self.end
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn pinned(&self) -> bool {
        self.pinned
    }
}

/// **Pure.** The retention candidate a snapshot makes, or `None` when it holds no
/// kopia manifest to keep (a `Pending` run, or an `Unchanged` one, which takes no
/// GFS slot at all).
fn retention_candidate(snap: &Snapshot) -> Option<RetentionCandidate> {
    let status = snap.status.as_ref()?;
    let id = status.snapshot.as_ref()?.kopia_snapshot_id.clone();
    let end = status
        .timing
        .as_ref()
        .and_then(|t| t.end_time.as_deref())
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))?;
    Some(RetentionCandidate {
        id,
        end,
        pinned: status.pinned.unwrap_or(snap.spec.pin),
    })
}

/// **Pure.** Whether today's retention would keep this snapshot, and which rules
/// say so.
///
/// The reasons are derived by re-running [`select_kept`] with one rule enabled
/// at a time. GFS buckets are independent — the kept set is their union — so a
/// single-rule run answers "would *this* rule alone have kept it" exactly, with
/// no second implementation of the bucketing to drift.
///
/// `None` when the snapshot holds no manifest, or the policy configures no
/// retention: there is no answer to preview, and inventing "kept: true" would
/// read as a guarantee.
pub fn view_retention_preview(
    snap: &Snapshot,
    policy: &SnapshotPolicy,
    peers: &[Arc<Snapshot>],
    now: DateTime<Utc>,
) -> Option<RetentionPreview> {
    let retention = policy.spec.retention.as_ref()?;
    let this = retention_candidate(snap)?;
    let target = this.id.clone();

    let mut candidates: Vec<RetentionCandidate> = peers
        .iter()
        .filter_map(|p| retention_candidate(p))
        .collect();
    if !candidates.iter().any(|c| c.id == target) {
        candidates.push(this);
    }

    let kept = select_kept(&candidates, retention).keep.contains(&target);

    let mut reasons = Vec::new();
    if candidates
        .iter()
        .any(|c| c.id == target && SnapshotLike::pinned(c))
    {
        reasons.push("pinned".to_string());
    }
    // Rule attribution runs over the same population with every pin dropped: a
    // pinned snapshot lands in `keep` whatever the rule says, so leaving the
    // pins in would credit each configured rule with a keep it did not make.
    let unpinned: Vec<RetentionCandidate> = candidates
        .iter()
        .map(|c| RetentionCandidate {
            id: c.id.clone(),
            end: c.end,
            pinned: false,
        })
        .collect();
    for (name, only) in single_rule_policies(retention) {
        if select_kept(&unpinned, &only).keep.contains(&target) {
            reasons.push(name.to_string());
        }
    }
    // A pin keeps a snapshot no bucket selected, so `pinned` alone is a
    // complete reason; every other kept snapshot has at least one rule.
    Some(RetentionPreview {
        kept,
        reasons,
        computed_at: now.to_rfc3339(),
    })
}

/// **Pure.** Each configured GFS rule on its own, for reason attribution.
///
/// Exhaustive by construction: every field of
/// [`Retention`](kopiur_api::common::Retention) appears, so a new rule is a
/// compile error here rather than a silently unattributed keep.
fn single_rule_policies(
    retention: &kopiur_api::common::Retention,
) -> Vec<(&'static str, kopiur_api::common::Retention)> {
    let blank = kopiur_api::common::Retention {
        keep_latest: None,
        keep_hourly: None,
        keep_daily: None,
        keep_weekly: None,
        keep_monthly: None,
        keep_annual: None,
    };
    let mut out = Vec::new();
    if retention.keep_latest.is_some() {
        out.push((
            "keepLatest",
            kopiur_api::common::Retention {
                keep_latest: retention.keep_latest,
                ..blank.clone()
            },
        ));
    }
    if retention.keep_hourly.is_some() {
        out.push((
            "keepHourly",
            kopiur_api::common::Retention {
                keep_hourly: retention.keep_hourly,
                ..blank.clone()
            },
        ));
    }
    if retention.keep_daily.is_some() {
        out.push((
            "keepDaily",
            kopiur_api::common::Retention {
                keep_daily: retention.keep_daily,
                ..blank.clone()
            },
        ));
    }
    if retention.keep_weekly.is_some() {
        out.push((
            "keepWeekly",
            kopiur_api::common::Retention {
                keep_weekly: retention.keep_weekly,
                ..blank.clone()
            },
        ));
    }
    if retention.keep_monthly.is_some() {
        out.push((
            "keepMonthly",
            kopiur_api::common::Retention {
                keep_monthly: retention.keep_monthly,
                ..blank.clone()
            },
        ));
    }
    if retention.keep_annual.is_some() {
        out.push((
            "keepAnnual",
            kopiur_api::common::Retention {
                keep_annual: retention.keep_annual,
                ..blank
            },
        ));
    }
    out
}

/// **Pure.** Whether the UI may open a browse session against this snapshot, and
/// why not when it may not.
///
/// Browsing needs a kopia manifest that still exists, which is exactly the
/// terminal-with-content phases. Exhaustive over [`SnapshotPhase`] — a new phase
/// must state whether its content is readable before it compiles.
pub fn browsability(snap: &Snapshot) -> (bool, Option<String>) {
    let status = snap.status.as_ref();
    let phase = status.and_then(|s| s.phase.as_ref());
    let has_id = status
        .and_then(|s| s.snapshot.as_ref())
        .is_some_and(|i| !i.kopia_snapshot_id.is_empty());

    let blocker = match phase {
        Some(SnapshotPhase::Succeeded) | Some(SnapshotPhase::Discovered) => {
            if has_id {
                None
            } else {
                Some(
                    "This snapshot has no kopia manifest ID recorded yet, so there is nothing to \
                     open. Wait for the operator to finish reconciling it."
                        .to_string(),
                )
            }
        }
        Some(SnapshotPhase::Pending) => {
            Some("This backup has not started, so no files exist to browse yet.".to_string())
        }
        Some(SnapshotPhase::Running) => Some(
            "This backup is still running. Browsing becomes available once it succeeds."
                .to_string(),
        ),
        Some(SnapshotPhase::Failed) => Some(
            "This backup failed, so it wrote no snapshot to browse. Check the failure detail \
             below, fix the cause, and run it again."
                .to_string(),
        ),
        Some(SnapshotPhase::Deleting) => Some(
            "This snapshot is being deleted from the repository, so its contents are on their \
             way out."
                .to_string(),
        ),
        Some(SnapshotPhase::Unchanged) => Some(
            "Nothing changed since the previous backup, so no new snapshot was written — browse \
             the previous one instead."
                .to_string(),
        ),
        Some(SnapshotPhase::Unknown(raw)) => Some(format!(
            "This snapshot is in phase {raw}, which this version of the kopiur UI does not \
             recognize. Upgrade kopiur-ui to the operator's version to browse it."
        )),
        None => Some(
            "The operator has not reported on this snapshot yet, so there is nothing to open."
                .to_string(),
        ),
    };
    (blocker.is_none(), blocker)
}

/// **Pure.** Everything the snapshot detail screen shows.
pub fn view_detail(
    snap: &Snapshot,
    siblings: &[Arc<Snapshot>],
    policy: Option<&SnapshotPolicy>,
    policy_snapshots: &[Arc<Snapshot>],
    now: DateTime<Utc>,
) -> SnapshotDetail {
    let status = snap.status.as_ref();
    let (browsable, browse_blocker) = browsability(snap);
    let conditions = status.map(|s| s.conditions.as_slice()).unwrap_or_default();
    SnapshotDetail {
        row: view_row(snap),
        stats: status.and_then(|s| s.stats.as_ref()).map(stats_view),
        duration_seconds: status
            .and_then(|s| s.timing.as_ref())
            .and_then(|t| t.duration_seconds),
        sources: status
            .and_then(|s| s.resolved.as_ref())
            .map(|r| {
                r.sources
                    .iter()
                    .filter_map(|s| s.source_path.clone().or_else(|| s.pvc.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        lineage: view_lineage(snap, siblings),
        retention_preview: policy
            .and_then(|p| view_retention_preview(snap, p, policy_snapshots, now)),
        failure: status.and_then(|s| s.failure.as_ref()).map(failure_view),
        log_tail: status
            .and_then(|s| s.log_tail.as_deref())
            .map(|t| t.lines().map(redact_text).collect())
            .unwrap_or_default(),
        conditions: conditions_view(conditions),
        gates: gate_hits(conditions, GateScope::covers_snapshot),
        browsable,
        browse_blocker,
    }
}

/// Turn the query string into a filter, resolving the repository against the
/// cluster (its UID is what discovered snapshots carry as a label).
async fn parse_query(
    app: &AppState,
    client: kube::Client,
    q: &SnapshotQuery,
) -> Result<ParsedFilter, ApiError> {
    let repository = match &q.repository {
        None => None,
        Some(name) => {
            let kind = q
                .repository_kind
                .unwrap_or(RepositoryKindPath::Repository)
                .kind();
            // The repository lives where the caller says, else in the listing's
            // namespace: a `?repository=` with no location on a cluster-wide
            // list would otherwise resolve against the operator's namespace,
            // which is almost never where the repository is.
            let repo_namespace = match kind {
                RepositoryKind::Repository => q
                    .repository_namespace
                    .as_deref()
                    .or(q.namespace.as_deref())
                    .ok_or_else(repository_namespace_required)?,
                RepositoryKind::ClusterRepository => "",
            };
            let ctx = ops_ctx(&app.cfg, client, Some(repo_namespace));
            Some(
                resolve_repo_filter_for(
                    &ctx,
                    name,
                    kind,
                    match kind {
                        RepositoryKind::Repository => Some(repo_namespace),
                        RepositoryKind::ClusterRepository => None,
                    },
                )
                .await?,
            )
        }
    };

    let origin = match &q.origin {
        None => None,
        Some(value) => Some(Origin::parse(value).ok_or_else(|| bad_origin(value))?),
    };
    let phase = match &q.phase {
        None => None,
        Some(value) => Some(parse_phase(value).ok_or_else(|| bad_phase(value))?),
    };

    Ok(ParsedFilter {
        repository,
        // The same value `--policy`/`--origin` build, so `matches_filter` and
        // the CLI's `label_selector` select the same rows.
        labels: SnapshotListFilter {
            policy: q.policy.clone(),
            origin,
        },
        phase,
    })
}

/// `GET /api/v1/snapshots?...`
async fn list(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    Query(q): Query<SnapshotQuery>,
) -> Result<Json<Page<SnapshotRow>>, ApiError> {
    let client = client_for(&app, &id)?;
    let filter = parse_query(&app, client.clone(), &q).await?;
    let snapshots = app
        .source
        .list::<Snapshot>(&id, &client, q.namespace.as_deref())
        .await?;

    let matched = filter_rows(&snapshots, &filter);
    if matched.len() > app.cfg.snapshot_list_cap {
        return Err(list_too_large(
            "snapshots",
            matched.len(),
            app.cfg.snapshot_list_cap,
        ));
    }

    let rows: Vec<SnapshotRow> = matched.iter().map(|s| view_row(s)).collect();
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    Ok(Json(paginate(rows, q.offset.unwrap_or(0), limit)))
}

/// `GET /api/v1/snapshots/{namespace}/{name}`
async fn detail(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<SnapshotDetail>, ApiError> {
    let client = client_for(&app, &id)?;
    let snap = app
        .source
        .get::<Snapshot>(&id, &client, Some(&namespace), &name)
        .await?
        .ok_or_else(|| snapshot_not_found(&namespace, &name))?;

    // Siblings are read in the snapshot's own namespace: a replication copy CR
    // is minted next to the repository it lands in, and the lineage screen shows
    // what this caller can actually navigate to.
    let siblings = app
        .source
        .list::<Snapshot>(&id, &client, Some(&namespace))
        .await?;

    let policy = match resolve_policy(&app, &id, &client, &snap, &namespace).await? {
        Some(policy) => policy,
        None => {
            return Ok(Json(view_detail(&snap, &siblings, None, &[], Utc::now())));
        }
    };
    // The retention preview must see the same population the operator's prune
    // does: every snapshot carrying this policy's config label.
    let policy_name = policy.metadata.name.clone().unwrap_or_default();
    let peers: Vec<Arc<Snapshot>> = siblings
        .iter()
        .filter(|s| policy_of(s) == Some(policy_name.as_str()))
        .cloned()
        .collect();

    Ok(Json(view_detail(
        &snap,
        &siblings,
        Some(&policy),
        &peers,
        Utc::now(),
    )))
}

/// The `SnapshotPolicy` governing this snapshot, when it names one the caller can
/// read.
///
/// A missing or unreadable policy is not an error: a discovered snapshot has no
/// policy at all, and one whose recipe was deleted still has a detail screen —
/// it simply has no retention preview.
async fn resolve_policy(
    app: &AppState,
    id: &Identity,
    client: &kube::Client,
    snap: &Snapshot,
    namespace: &str,
) -> Result<Option<Arc<SnapshotPolicy>>, ApiError> {
    let Some(policy_ref) = snap.spec.policy_ref.as_ref() else {
        return Ok(None);
    };
    let policy_ns = policy_ref.namespace.as_deref().unwrap_or(namespace);
    match app
        .source
        .get::<SnapshotPolicy>(id, client, Some(policy_ns), &policy_ref.name)
        .await
    {
        Ok(found) => Ok(found),
        // A caller who may read the snapshot but not its recipe sees the
        // snapshot, minus the part the recipe would have told them.
        Err(e) if e.kind() == kopiur_ops::OpsErrorKind::Forbidden => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The 400 for a `?repository=` with nowhere to look it up.
fn repository_namespace_required() -> ApiError {
    problem(
        400,
        "repository-namespace-required",
        "Filtering by a namespaced Repository needs to know which namespace it is in.",
        "The request named a repository but neither ?repositoryNamespace= nor ?namespace=, and \
         two namespaces may each hold a repository with that name.",
        "add ?repositoryNamespace=<namespace>, or filter with ?repositoryKind=cluster-repository \
         if you meant the cluster-scoped one",
    )
}

/// The 400 for an origin nothing produces.
fn bad_origin(value: &str) -> ApiError {
    problem(
        400,
        "invalid-filter",
        format!("`{value}` is not a snapshot origin."),
        "The origin filter takes the value the rows themselves carry, and this is not one of \
         them.",
        "use one of scheduled, manual, discovered, adopted or replicated",
    )
}

/// The 400 for a phase nothing produces.
fn bad_phase(value: &str) -> ApiError {
    problem(
        400,
        "invalid-filter",
        format!("`{value}` is not a snapshot phase."),
        "The phase filter takes the value the rows themselves carry, and this is not one of \
         them.",
        "use one of pending, running, succeeded, failed, deleting, discovered, unchanged, or \
         unknown to find snapshots in a phase this UI build does not recognize",
    )
}

/// The 404 for a snapshot that is not there.
fn snapshot_not_found(namespace: &str, name: &str) -> ApiError {
    problem(
        404,
        "not-found",
        format!("There is no Snapshot called {name} in namespace {namespace}."),
        "It was pruned by retention, deleted, or never existed — the SPA may be showing a link \
         from a listing taken before the change.",
        "reload the snapshots list to see what the cluster holds now",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::testutil::from_yaml;
    use kopiur_ui_model::views::{OriginView, SnapshotPhaseView};

    /// A snapshot, spelled with only the fields a test cares about.
    fn snapshot(yaml: &str) -> Arc<Snapshot> {
        Arc::new(from_yaml(yaml))
    }

    /// A snapshot as the operator actually produces one: BOTH the config and
    /// origin labels (the operator mirrors `status.origin` onto the label) plus
    /// the status. The label pair is what the CLI's server-side selector filters
    /// on, so a fixture carrying only the status would not exercise the filter
    /// the two front ends now share.
    fn nightly_run(name: &str, start: &str, phase: &str) -> Arc<Snapshot> {
        snapshot(&format!(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: {name}
  namespace: media
  labels:
    "kopiur.home-operations.com/config": nightly
    "kopiur.home-operations.com/origin": scheduled
spec:
  policyRef: {{ name: nightly }}
status:
  phase: {phase}
  origin: scheduled
  timing: {{ startTime: "{start}", endTime: "{start}" }}
  snapshot:
    kopiaSnapshotID: k-{name}
    identity: {{ username: kopiur, hostname: media, sourcePath: /data }}
  resolved:
    repository: {{ kind: Repository, name: nas, namespace: media }}
"#
        ))
    }

    #[test]
    fn a_row_flattens_the_status_the_table_shows() {
        let snap = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: nightly-1
  namespace: media
  labels: { "kopiur.home-operations.com/config": nightly }
spec:
  policyRef: { name: nightly }
  deletionPolicy: Retain
status:
  phase: Succeeded
  origin: scheduled
  pinned: true
  timing: { startTime: "2026-09-08T02:00:00Z", endTime: "2026-09-08T02:04:00Z" }
  snapshot:
    kopiaSnapshotID: k123
    identity: { username: kopiur, hostname: media, sourcePath: /data }
  stats: { sizeBytes: 100, bytesNew: 20, filesNew: 3, filesModified: 2, filesUnchanged: 95, filesFailed: 1 }
  resolved:
    repository: { kind: Repository, name: nas, namespace: media }
"#,
        );
        let row = view_row(&snap);
        assert_eq!(row.namespace, "media");
        assert_eq!(row.name, "nightly-1");
        assert_eq!(row.phase, Some(SnapshotPhaseView::Succeeded));
        assert_eq!(row.origin, Some(OriginView::Scheduled));
        assert_eq!(row.policy.as_deref(), Some("nightly"));
        assert_eq!(row.repository.as_deref(), Some("Repository/media/nas"));
        assert_eq!(row.kopia_snapshot_id.as_deref(), Some("k123"));
        assert_eq!(row.identity.as_deref(), Some("kopiur@media:/data"));
        assert_eq!(row.size_bytes, Some(100));
        assert_eq!(row.files_total, Some(100), "new + modified + unchanged");
        assert_eq!(row.files_failed, Some(1));
        assert!(row.pinned);
        assert_eq!(row.deletion_policy.as_deref(), Some("Retain"));
    }

    #[test]
    fn the_row_names_the_same_policy_the_cli_prints() {
        // `policy_of` is `kopiur_ops`', shared with `kubectl kopiur snapshots`:
        // the spec reference first, the config label second. This endpoint used
        // to have the precedence the other way round, so one row could name two
        // different policies depending on which front end drew it.
        let both = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: adopted-1
  namespace: media
  labels: { "kopiur.home-operations.com/config": relabelled }
spec:
  policyRef: { name: as-written }
status: { phase: Discovered, origin: adopted }
"#,
        );
        assert_eq!(view_row(&both).policy.as_deref(), Some("as-written"));

        let label_only = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: adopted-2
  namespace: media
  labels: { "kopiur.home-operations.com/config": nightly }
spec: {}
status: { phase: Discovered, origin: adopted }
"#,
        );
        assert_eq!(
            view_row(&label_only).policy.as_deref(),
            Some("nightly"),
            "an adopted row with no spec reference is named by its label"
        );

        let neither = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: { name: found-1, namespace: media }
spec: {}
status: { phase: Discovered, origin: discovered }
"#,
        );
        assert_eq!(view_row(&neither).policy, None);
    }

    #[test]
    fn a_discovered_snapshot_takes_its_repository_from_its_owner() {
        let discovered = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: found-1
  namespace: media
  ownerReferences:
    - apiVersion: kopiur.home-operations.com/v1alpha1
      kind: Repository
      name: nas
      uid: abc
      controller: true
spec: {}
status: { phase: Discovered, origin: discovered }
"#,
        );
        assert_eq!(
            view_row(&discovered).repository.as_deref(),
            Some("Repository/media/nas"),
            "a discovered row has neither pin, so the ownerReference answers"
        );
    }

    #[test]
    fn phase_filters_accept_the_wire_names_and_unknown() {
        assert_eq!(
            parse_phase("succeeded"),
            Some(PhaseFilter::Exactly(SnapshotPhase::Succeeded))
        );
        assert_eq!(
            parse_phase("unchanged"),
            Some(PhaseFilter::Exactly(SnapshotPhase::Unchanged)),
            "#351's phase must be filterable, not invisible"
        );
        assert_eq!(parse_phase("unknown"), Some(PhaseFilter::Unrecognized));
        assert_eq!(parse_phase("Succeeded"), None, "the wire name is camelCase");
        assert_eq!(parse_phase("nonsense"), None);
    }

    #[test]
    fn the_unknown_phase_filter_selects_exactly_the_unrecognized_rows() {
        let odd = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: { name: odd, namespace: media }
spec: {}
status: { phase: Quiescing }
"#,
        );
        let normal = nightly_run("nightly-1", "2026-09-08T02:00:00Z", "Succeeded");
        let all = vec![odd.clone(), normal.clone()];

        let unrecognized = filter_rows(
            &all,
            &ParsedFilter {
                phase: Some(PhaseFilter::Unrecognized),
                ..Default::default()
            },
        );
        assert_eq!(unrecognized.len(), 1);
        assert_eq!(unrecognized[0].metadata.name.as_deref(), Some("odd"));

        let succeeded = filter_rows(
            &all,
            &ParsedFilter {
                phase: Some(PhaseFilter::Exactly(SnapshotPhase::Succeeded)),
                ..Default::default()
            },
        );
        assert_eq!(succeeded.len(), 1);
        assert_eq!(succeeded[0].metadata.name.as_deref(), Some("nightly-1"));
    }

    #[test]
    fn filters_compose_and_the_result_is_newest_first() {
        let older = nightly_run("nightly-1", "2026-09-06T02:00:00Z", "Succeeded");
        let newer = nightly_run("nightly-2", "2026-09-08T02:00:00Z", "Succeeded");
        let failed = nightly_run("nightly-3", "2026-09-07T02:00:00Z", "Failed");
        let other_policy = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: weekly-1
  namespace: media
  labels: { "kopiur.home-operations.com/config": weekly }
spec: { policyRef: { name: weekly } }
status:
  phase: Succeeded
  origin: manual
  timing: { startTime: "2026-09-09T02:00:00Z" }
"#,
        );
        let all = vec![older, newer, failed, other_policy];

        let rows = filter_rows(&all, &ParsedFilter::default());
        let names: Vec<&str> = rows
            .iter()
            .map(|s| s.metadata.name.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(
            names,
            vec!["weekly-1", "nightly-2", "nightly-3", "nightly-1"],
            "newest run first"
        );

        let scoped = filter_rows(
            &all,
            &ParsedFilter {
                labels: SnapshotListFilter {
                    policy: Some("nightly".into()),
                    origin: Some(Origin::Scheduled),
                },
                phase: Some(PhaseFilter::Exactly(SnapshotPhase::Succeeded)),
                ..Default::default()
            },
        );
        let names: Vec<&str> = scoped
            .iter()
            .map(|s| s.metadata.name.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(names, vec!["nightly-2", "nightly-1"]);
    }

    #[test]
    fn a_repository_filter_matches_the_pin_and_the_discovered_label() {
        let pinned = nightly_run("nightly-1", "2026-09-08T02:00:00Z", "Succeeded");
        let labelled = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: found-1
  namespace: media
  labels: { "kopiur.home-operations.com/repository-uid": uid-nas }
spec: {}
status: { phase: Discovered, origin: discovered }
"#,
        );
        let elsewhere = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: { name: other-1, namespace: media }
spec: {}
status:
  phase: Succeeded
  resolved: { repository: { kind: Repository, name: elsewhere, namespace: media } }
"#,
        );
        let filter = ParsedFilter {
            repository: Some(RepoFilter {
                uid: "uid-nas".into(),
                name: "nas".into(),
                kind: RepositoryKind::Repository,
                namespace: Some("media".into()),
            }),
            ..Default::default()
        };
        let rows = filter_rows(&[pinned, labelled, elsewhere], &filter);
        let mut names: Vec<&str> = rows
            .iter()
            .map(|s| s.metadata.name.as_deref().unwrap_or_default())
            .collect();
        names.sort_unstable();
        assert_eq!(names, vec!["found-1", "nightly-1"]);
    }

    #[test]
    fn pagination_windows_the_filtered_set() {
        let rows: Vec<SnapshotRow> = (0..7)
            .map(|i| {
                view_row(&nightly_run(
                    &format!("n-{i}"),
                    "2026-09-08T02:00:00Z",
                    "Succeeded",
                ))
            })
            .collect();
        let page = paginate(rows, 5, 50);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total, 7);
    }

    #[test]
    fn lineage_correlates_a_copy_by_its_source_manifest_id() {
        let source = nightly_run("nightly-1", "2026-09-08T02:00:00Z", "Succeeded");
        let copy = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: { name: nightly-1-copy, namespace: media }
spec: {}
status:
  phase: Succeeded
  origin: replicated
  snapshot:
    kopiaSnapshotID: k-different
    identity: { username: kopiur, hostname: media, sourcePath: /data }
  copiedFrom:
    repository: { kind: Repository, name: nas, namespace: media }
    sourceManifestId: k-nightly-1
    startTime: "2026-09-08T02:00:00Z"
"#,
        );
        let siblings = vec![source.clone(), copy.clone()];

        let of_source = view_lineage(&source, &siblings);
        assert_eq!(of_source.copied_from_repository, None);
        assert_eq!(
            of_source.copies,
            vec![SnapshotRefView {
                namespace: "media".into(),
                name: "nightly-1-copy".into()
            }],
            "the source knows what was copied out of it"
        );

        let of_copy = view_lineage(&copy, &siblings);
        assert_eq!(
            of_copy.copied_from_repository.as_deref(),
            Some("Repository/media/nas")
        );
        assert_eq!(of_copy.source_manifest_id.as_deref(), Some("k-nightly-1"));
        assert!(
            of_copy.copies.is_empty(),
            "a copy of a copy would have to record this one's id, and none does"
        );
    }

    #[test]
    fn the_retention_preview_matches_the_prune_that_would_run() {
        let policy: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
  retention: { keepDaily: 2 }
"#,
        );
        let d24 = nightly_run("d24", "2026-05-24T02:00:00Z", "Succeeded");
        let d23 = nightly_run("d23", "2026-05-23T02:00:00Z", "Succeeded");
        let d22 = nightly_run("d22", "2026-05-22T02:00:00Z", "Succeeded");
        let peers = vec![d24.clone(), d23.clone(), d22.clone()];
        let now = DateTime::parse_from_rfc3339("2026-05-24T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let kept = view_retention_preview(&d24, &policy, &peers, now).expect("a preview exists");
        assert!(kept.kept);
        assert_eq!(kept.reasons, vec!["keepDaily"]);
        assert!(kept.computed_at.starts_with("2026-05-24T12:00:00"));

        let pruned = view_retention_preview(&d22, &policy, &peers, now).unwrap();
        assert!(
            !pruned.kept,
            "the third-newest day falls outside keepDaily: 2"
        );
        assert!(pruned.reasons.is_empty());
    }

    #[test]
    fn a_pinned_snapshot_is_kept_and_says_why() {
        let policy: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
  retention: { keepDaily: 1 }
"#,
        );
        let newest = nightly_run("d24", "2026-05-24T02:00:00Z", "Succeeded");
        let old_pinned = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: keepme
  namespace: media
  labels: { "kopiur.home-operations.com/config": nightly }
spec: { policyRef: { name: nightly }, pin: true }
status:
  phase: Succeeded
  origin: manual
  pinned: true
  timing: { startTime: "2026-01-01T02:00:00Z", endTime: "2026-01-01T02:00:00Z" }
  snapshot:
    kopiaSnapshotID: k-keepme
    identity: { username: kopiur, hostname: media, sourcePath: /data }
"#,
        );
        let peers = vec![newest, old_pinned.clone()];
        let preview = view_retention_preview(&old_pinned, &policy, &peers, Utc::now()).unwrap();
        assert!(preview.kept, "a pin exempts a snapshot from GFS entirely");
        assert_eq!(preview.reasons, vec!["pinned"]);
    }

    #[test]
    fn no_retention_configured_means_no_preview_rather_than_a_confident_yes() {
        let policy: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
"#,
        );
        let snap = nightly_run("d24", "2026-05-24T02:00:00Z", "Succeeded");
        assert!(
            view_retention_preview(&snap, &policy, std::slice::from_ref(&snap), Utc::now())
                .is_none()
        );
    }

    #[test]
    fn a_snapshot_with_no_manifest_has_no_retention_preview() {
        let policy: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
  retention: { keepDaily: 2 }
"#,
        );
        let pending = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: { name: pending-1, namespace: media }
spec: { policyRef: { name: nightly } }
status: { phase: Pending }
"#,
        );
        assert!(view_retention_preview(&pending, &policy, &[], Utc::now()).is_none());
    }

    #[test]
    fn every_phase_answers_whether_it_can_be_browsed() {
        let cases = [
            ("Succeeded", true),
            ("Discovered", true),
            ("Pending", false),
            ("Running", false),
            ("Failed", false),
            ("Deleting", false),
            ("Unchanged", false),
            ("Quiescing", false),
        ];
        for (phase, expected) in cases {
            let snap = snapshot(&format!(
                r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: {{ name: s, namespace: media }}
spec: {{}}
status:
  phase: {phase}
  snapshot:
    kopiaSnapshotID: k1
    identity: {{ username: kopiur, hostname: media, sourcePath: /data }}
"#
            ));
            let (browsable, blocker) = browsability(&snap);
            assert_eq!(browsable, expected, "phase {phase}");
            assert_eq!(
                blocker.is_none(),
                expected,
                "phase {phase} must explain itself when it cannot be browsed"
            );
        }
    }

    #[test]
    fn a_succeeded_snapshot_with_no_manifest_id_is_not_browsable() {
        let snap = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: { name: s, namespace: media }
spec: {}
status: { phase: Succeeded }
"#,
        );
        let (browsable, blocker) = browsability(&snap);
        assert!(!browsable);
        assert!(
            blocker.unwrap().contains("manifest"),
            "the blocker says what is missing, not just 'no'"
        );
    }

    #[test]
    fn the_log_tail_and_failure_message_are_redacted() {
        let leaky = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: { name: failed-1, namespace: media }
spec: {}
status:
  phase: Failed
  logTail: "connecting\nAWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI rejected"
  failure:
    kopiaErrorClass: RepositoryUnreachable
    message: "auth failed: AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI"
    retryRecommended: true
    exitCode: 1
    op: "repository connect"
"#,
        );
        let detail = view_detail(&leaky, &[], None, &[], Utc::now());
        let joined = detail.log_tail.join("\n");
        assert!(
            !joined.contains("wJalrXUtnFEMI"),
            "a credential quoted back by kopia must not reach the browser: {joined}"
        );
        assert!(
            joined.contains("AWS_SECRET_ACCESS_KEY=***"),
            "the name survives so the user can see WHICH credential was rejected: {joined}"
        );
        let failure = detail.failure.expect("a failed run has failure detail");
        assert!(
            !failure
                .message
                .unwrap_or_default()
                .contains("wJalrXUtnFEMI"),
            "the failure message is redacted too"
        );
        assert_eq!(
            failure.kopia_error_class.as_deref(),
            Some("RepositoryUnreachable")
        );
        assert_eq!(failure.op.as_deref(), Some("repository connect"));
        assert_eq!(failure.retry_recommended, Some(true));
    }

    #[test]
    fn the_detail_lists_the_sources_the_run_actually_covered() {
        let snap = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: { name: nightly-1, namespace: media }
spec: {}
status:
  phase: Succeeded
  timing: { startTime: "2026-09-08T02:00:00Z", endTime: "2026-09-08T02:04:00Z", durationSeconds: 240 }
  snapshot:
    kopiaSnapshotID: k1
    identity: { username: kopiur, hostname: media, sourcePath: /data }
  resolved:
    sources:
      - { pvc: "media/data", sourcePath: "/data" }
      - { pvc: "media/config" }
"#,
        );
        let detail = view_detail(&snap, &[], None, &[], Utc::now());
        assert_eq!(detail.sources, vec!["/data", "media/config"]);
        assert_eq!(detail.duration_seconds, Some(240));
        assert!(detail.browsable);
        assert_eq!(detail.browse_blocker, None);
        assert!(detail.retention_preview.is_none(), "no policy was resolved");
    }

    #[test]
    fn the_filter_problems_name_the_values_that_would_work() {
        let origin = bad_origin("nope");
        assert_eq!(origin.0.status, 400);
        assert!(origin.0.fix.contains("scheduled"), "got {}", origin.0.fix);

        let phase = bad_phase("Succeeded");
        assert_eq!(phase.0.status, 400);
        assert!(
            phase.0.fix.contains("succeeded") && phase.0.fix.contains("unknown"),
            "got {}",
            phase.0.fix
        );

        let ns = repository_namespace_required();
        assert_eq!(ns.0.status, 400);
        assert!(ns.0.fix.contains("repositoryNamespace"));
    }
}
