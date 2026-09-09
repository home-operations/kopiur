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

use axum::extract::State;
use axum::{Json, Router, routing::get};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use kopiur_api::common::RepositoryKind;
use kopiur_api::gates::GateScope;
use kopiur_api::retention::{
    SnapshotRetentionView, retention_buckets, retention_group_key, retention_view, select_kept,
};
use kopiur_api::snapshot::repository_ref_for;
use kopiur_api::snapshot_policy::is_multi_repo;
use kopiur_api::{Origin, Snapshot, SnapshotPhase, SnapshotPolicy};
use kopiur_ops::snapshots::{
    RepoFilter, SnapshotListFilter, matches_filter, matches_repository, policy_of,
    resolve_repo_filter_for, sort_key,
};
use kopiur_ui_model::views::{
    FailureView, Lineage, Page, PolicyRef, RetentionBucket, RetentionCandidate, RetentionPlan,
    RetentionPreview, SnapshotDetail, SnapshotRefView, SnapshotRow, SnapshotStatsView,
};

use crate::AppState;
use crate::api::problem::{ApiError, problem};
use crate::api::{
    RepositoryKindPath, UiPath, UiQuery, client_for, conditions_view, gate_hits, list_too_large,
    ops_ctx, origin_view, paginate, repo_ref_display, snapshot_phase_view,
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
        .route("/snapshots/{namespace}/{name}/retention", get(retention))
}

/// The snapshot table's query string.
///
/// `deny_unknown_fields`: with nine optional parameters, a mistyped one is the
/// likeliest mistake a caller makes, and silently ignoring it would return a
/// wider set than was asked for — a snapshots table showing another
/// repository's rows under this repository's heading.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
pub fn failure_view(f: &kopiur_api::common::FailureBlock) -> FailureView {
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

/// **Pure.** Is this `Snapshot` in the population the prune will evaluate for
/// `policy_name`?
///
/// # The prune's rule, and only the prune's rule
///
/// `kopiur_controller::snapshot_policy` enumerates a policy's children with
/// `io::snapshot_children(ctx, &namespace, CONFIG_LABEL, &name)` — a
/// `CONFIG_LABEL=<policy>` selector, in the **policy's** namespace. So that is
/// what this asks, through [`matches_filter`], which `kopiur_ops` pins against
/// the server-side `label_selector` the CLI sends.
///
/// Deliberately **not** [`policy_of`], which prefers `spec.policyRef.name` and
/// falls back to the label. `policy_of` is the right answer to "which policy
/// does this row *display* as" — the user wrote the spec — but it is the wrong
/// answer to "will the prune for this policy evaluate this row". The two
/// disagree exactly mid-adoption, when a discovered snapshot's `spec.policyRef`
/// has been set and its label has not yet been rewritten: `policy_of` puts the
/// row in the new policy's buckets, where it competes for keep slots against
/// rows the operator is about to prune, while the operator's own selection has
/// never heard of it. A preview that answers about a set the prune will not
/// evaluate is the one thing this endpoint exists not to do.
pub fn selected_by_policy(snap: &Snapshot, policy_name: &str) -> bool {
    matches_filter(
        snap,
        &SnapshotListFilter {
            policy: Some(policy_name.to_string()),
            origin: None,
        },
    )
}

/// **Pure.** [`selected_by_policy`] over a candidate set — the rows the prune
/// would evaluate, out of the rows the caller listed.
pub fn policy_population<'a>(
    candidates: &'a [Arc<Snapshot>],
    policy_name: &str,
) -> impl Iterator<Item = &'a Snapshot> {
    // Owned so the returned iterator's lifetime is the candidates' alone; the
    // one allocation is per call, not per row.
    let wanted = policy_name.to_string();
    candidates
        .iter()
        .map(Arc::as_ref)
        .filter(move |s| selected_by_policy(s, &wanted))
}

/// **Pure.** Whether today's retention would keep this snapshot, and which rules
/// say so.
///
/// # The population and the bucketing are the prune's, not a second opinion
///
/// `kopiur_api::retention::retention_buckets` is what
/// `kopiur_controller::snapshot_policy::backups_to_delete` runs over: it admits
/// only `Succeeded` rows carrying controller-written provenance, excludes
/// terminating ones, falls back to `creationTimestamp` for a missing `endTime`,
/// reads `spec.pin` (never `status.pinned` — during an unpin the two disagree,
/// and the prune honours the spec), and splits the population **per source**,
/// and per `(source, repository)` while the policy is multi-repo.
///
/// That last part is the whole reason this shares code rather than approximating.
/// A flat run over a 7-PVC `pvcSelector` fan-out under `keepDaily: 7` finds seven
/// keepers — one day across all seven volumes — and reports the other 42 real,
/// protected restore points as `kept: false`. That is the #346 shape the
/// controller's own comment calls "silent data loss introduced by the fan-out",
/// and telling a user their backup is about to be deleted when it is not is the
/// same lie in the other direction.
///
/// Only the *target's own bucket* is evaluated: buckets are independent, so the
/// other buckets cannot change this row's verdict.
///
/// `None` when the policy configures no retention, when this snapshot is not in
/// the GFS population at all (a `Pending`, `Failed`, `Unchanged` or `Deleting`
/// row), or when it is not in the *policy's* population by
/// [`selected_by_policy`] — there is no answer to preview, and inventing one
/// would read as a guarantee.
pub fn view_retention_preview(
    snap: &Snapshot,
    policy: &SnapshotPolicy,
    peers: &[Arc<Snapshot>],
    now: DateTime<Utc>,
) -> Option<RetentionPreview> {
    let retention = policy.spec.retention.as_ref()?;
    let policy_is_multi = is_multi_repo(&policy.spec);
    let policy_name = policy.metadata.name.clone().unwrap_or_default();

    // The id the prune works in is the CR name, not the kopia manifest id.
    let target = snap.metadata.name.clone()?;
    // Not in the population → not previewable. Asked before bucketing so a row
    // the prune ignores never gets an answer about a set it is not in. Both
    // gates: the GFS one (phase + provenance) and the policy one (the label the
    // prune selects on).
    retention_view(snap)?;
    if !selected_by_policy(snap, &policy_name) {
        return None;
    }
    let bucket_key = retention_group_key(snap, policy_is_multi);

    // The peer set may or may not already contain this snapshot, depending on
    // how the caller assembled it; a duplicate would let one row compete with
    // itself for its own keep slot.
    let mut population: Vec<&Snapshot> = policy_population(peers, &policy_name)
        .filter(|p| p.metadata.name.as_deref() != Some(target.as_str()))
        .collect();
    population.push(snap);

    let buckets = retention_buckets(&population, policy_is_multi);
    let bucket = buckets.get(&bucket_key)?;

    let kept = select_kept(bucket, retention).keep.contains(&target);

    let mut attribution = bucket_rules(bucket, retention);
    // A pin keeps a snapshot no bucket selected, so `pinned` alone is a
    // complete reason; every other kept snapshot has at least one rule.
    Some(RetentionPreview {
        kept,
        reasons: attribution.remove(&target).unwrap_or_default(),
        computed_at: now.to_rfc3339(),
    })
}

/// **Pure.** Which rules hold each row of one bucket, and in which slot.
///
/// Keyed by CR name — the id the prune works in — and in the policy's own rule
/// order (`keepLatest`, `keepHourly`, `keepDaily`, …) so two rows' reason lists
/// read the same way.
///
/// # Why one pass per rule rather than one pass per row
///
/// Attribution re-runs the selection over the same bucket with one rule enabled
/// at a time: GFS buckets are a union, so a single-rule run answers "would THIS
/// rule alone have kept it" exactly, with no second implementation of the
/// bucketing to drift from `select_kept`. Running that per row would be O(rules
/// × rows²) on a bucket that can hold thousands of snapshots; running it once
/// per rule and reading each row's *position* out of the result is O(rules ×
/// rows log rows) and gives the slot for free.
///
/// The slot is that position, 1-based, in the rule's newest-first keep list —
/// `keepDaily slot 3` is the third-newest day `keepDaily` holds. That is the
/// number a user needs: it says how close a restore point is to ageing out,
/// which a bare rule name does not.
///
/// # The pin flag is dropped for the attribution runs; the ROW is not
///
/// A pinned row lands in `keep` whatever the rule says, so running attribution
/// with the flag on would credit every configured rule with a keep it did not
/// make. The flag is therefore cleared — and `pinned` prepended as its own,
/// slotless reason, an exemption from bucketing rather than a place in one.
///
/// The row itself stays in the counterfactual, and that is deliberate: the real
/// prune's `keep_per_period` also walks pinned rows when it fills a bucket, so
/// dropping them here would shift every younger row's slot to a number the
/// operator will not use. The consequence is that a pinned snapshot which
/// today's rules would ALSO have kept reports both reasons — `["pinned",
/// "keepDaily slot 1"]` — which is the useful answer, because it says unpinning
/// would not lose it. Both wire docs state this.
pub fn bucket_rules(
    bucket: &[SnapshotRetentionView],
    retention: &kopiur_api::common::Retention,
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut out: std::collections::BTreeMap<String, Vec<String>> = bucket
        .iter()
        .filter(|v| v.pinned)
        .map(|v| (v.name.clone(), vec!["pinned".to_string()]))
        .collect();

    let unpinned: Vec<SnapshotRetentionView> = bucket
        .iter()
        .map(|v| SnapshotRetentionView {
            name: v.name.clone(),
            end_time: v.end_time,
            pinned: false,
        })
        .collect();
    for (name, only) in single_rule_policies(retention) {
        // `keep` is newest-first, so the index IS the slot.
        for (slot, id) in select_kept(&unpinned, &only).keep.iter().enumerate() {
            out.entry(id.clone())
                .or_default()
                .push(format!("{name} slot {}", slot + 1));
        }
    }
    out
}

/// **Pure.** The rows a plan for `snap` competes over: the policy's population,
/// with the subject deduplicated out and then pushed back in.
///
/// The peer set may or may not already contain the subject, depending on how the
/// caller assembled it, and a duplicate would let one row compete with itself
/// for its own keep slot.
///
/// One assembly, two readers — [`view_retention_plan`] renders it and
/// [`plan_candidate_count`] measures it — so the cap counts exactly the rows
/// that would be rendered rather than an approximation of them.
fn plan_population<'a>(
    snap: &'a Snapshot,
    policy_name: &str,
    peers: &'a [Arc<Snapshot>],
) -> Vec<&'a Snapshot> {
    let target = snap.metadata.name.as_deref();
    let mut population: Vec<&Snapshot> = policy_population(peers, policy_name)
        .filter(|p| p.metadata.name.as_deref() != target)
        .collect();
    population.push(snap);
    population
}

/// **Pure.** How many candidates a plan for `snap` would carry — the number
/// [`plan_too_large`] is measured against.
///
/// Counted through `retention_buckets`, so rows the prune would not evaluate
/// (wrong phase, no provenance, terminating) are not counted: the cap bounds the
/// *response*, and those rows never reach it.
pub fn plan_candidate_count(
    snap: &Snapshot,
    policy: &SnapshotPolicy,
    peers: &[Arc<Snapshot>],
) -> usize {
    let policy_name = policy.metadata.name.clone().unwrap_or_default();
    let population = plan_population(snap, &policy_name, peers);
    retention_buckets(&population, is_multi_repo(&policy.spec))
        .values()
        .map(Vec::len)
        .sum()
}

/// **Pure.** Every snapshot competing for the same buckets as `snap`, with the
/// verdict today's selection gives each.
///
/// The population, the bucketing and the selection are all the prune's own — see
/// [`view_retention_preview`] for why sharing them is the whole point — and this
/// simply reports EVERY bucket rather than only the subject's, because the
/// screen it feeds is "which snapshots will this policy keep", not "will it keep
/// this one".
///
/// `namespace` is the namespace `peers` was listed in — the **policy's**, which
/// is where the prune enumerates, and which every candidate therefore shares.
///
/// `None` when `snap` is not in the GFS population at all (a `Pending`,
/// `Failed`, `Unchanged` or `Deleting` row, or one with no controller-written
/// provenance), or when the prune for this policy would not select it
/// ([`selected_by_policy`]). The subject has to be in the set for the plan to be
/// *about* it; answering with a plan it does not appear in would read as "your
/// snapshot is being pruned".
pub fn view_retention_plan(
    snap: &Snapshot,
    namespace: &str,
    policy: &SnapshotPolicy,
    peers: &[Arc<Snapshot>],
    now: DateTime<Utc>,
) -> Option<RetentionPlan> {
    let target = snap.metadata.name.clone()?;
    retention_view(snap)?;
    let policy_name = policy.metadata.name.clone().unwrap_or_default();
    if !selected_by_policy(snap, &policy_name) {
        return None;
    }

    let policy_is_multi = is_multi_repo(&policy.spec);
    let population = plan_population(snap, &policy_name, peers);

    // An absent `spec.retention` is not "prune everything": the operator only
    // runs a selection when retention is configured, so the honest plan is every
    // candidate kept, flagged `unbounded` so the SPA can say why.
    let retention = policy.spec.retention.clone().unwrap_or_default();
    let unbounded = policy.spec.retention.is_none();

    let buckets = retention_buckets(&population, policy_is_multi)
        .into_iter()
        .map(|(key, mut rows)| {
            // The selection kernel's own order: newest first, ties broken by id.
            rows.sort_by(|a, b| {
                b.end_time
                    .cmp(&a.end_time)
                    .then_with(|| a.name.cmp(&b.name))
            });
            let kept: std::collections::BTreeSet<String> = if unbounded {
                rows.iter().map(|v| v.name.clone()).collect()
            } else {
                select_kept(&rows, &retention).keep.into_iter().collect()
            };
            let mut rules = if unbounded {
                std::collections::BTreeMap::new()
            } else {
                bucket_rules(&rows, &retention)
            };
            RetentionBucket {
                candidates: rows
                    .into_iter()
                    .map(|v| RetentionCandidate {
                        kept: kept.contains(&v.name),
                        rules: rules.remove(&v.name).unwrap_or_default(),
                        subject: v.name == target,
                        end_time: v.end_time.to_rfc3339(),
                        pinned: v.pinned,
                        namespace: namespace.to_string(),
                        name: v.name,
                    })
                    .collect(),
                key,
            }
        })
        .collect();

    Some(RetentionPlan {
        buckets,
        policy: PolicyRef {
            namespace: policy
                .metadata
                .namespace
                .clone()
                .unwrap_or_else(|| namespace.to_string()),
            name: policy.metadata.name.clone().unwrap_or_default(),
        },
        computed_at: now.to_rfc3339(),
        unbounded,
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
    UiQuery(q): UiQuery<SnapshotQuery>,
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
    UiPath((namespace, name)): UiPath<(String, String)>,
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
    // does — which is a LIST in the POLICY's namespace, not this snapshot's. The
    // two coincide for every ordinary snapshot, so the rows already in hand are
    // reused rather than re-listed; a cross-namespace `policyRef` (nothing in
    // `validate::snapshot` forbids one) takes the second read.
    let peers = policy_peers(
        &app,
        &id,
        &client,
        &policy,
        &namespace,
        Some((namespace.as_str(), &siblings)),
    )
    .await?;

    Ok(Json(view_detail(
        &snap,
        &siblings,
        Some(&policy),
        &peers,
        Utc::now(),
    )))
}

/// The rows the prune would evaluate for `policy`, read as the caller.
///
/// Two things this gets right that the old inline filter did not:
///
/// * **Where.** The controller enumerates a policy's children in the *policy's*
///   namespace. A `Snapshot` in `ns-a` naming a policy in `ns-b` was previously
///   previewed against `ns-a`'s rows while the operator pruned `ns-b`'s.
/// * **Which.** [`selected_by_policy`] — the `CONFIG_LABEL` the prune selects
///   on — rather than `policy_of`'s spec-first display precedence.
///
/// `already_listed` is a `(namespace, rows)` pair the caller has in hand; it is
/// used only when that namespace is the policy's, so the ordinary same-namespace
/// case costs no extra read.
async fn policy_peers(
    app: &AppState,
    id: &Identity,
    client: &kube::Client,
    policy: &SnapshotPolicy,
    fallback_namespace: &str,
    already_listed: Option<(&str, &[Arc<Snapshot>])>,
) -> Result<Vec<Arc<Snapshot>>, ApiError> {
    let policy_name = policy.metadata.name.clone().unwrap_or_default();
    let policy_ns = policy
        .metadata
        .namespace
        .as_deref()
        .unwrap_or(fallback_namespace);

    let listed = match already_listed {
        Some((ns, rows)) if ns == policy_ns => rows.to_vec(),
        _ => {
            app.source
                .list::<Snapshot>(id, client, Some(policy_ns))
                .await?
        }
    };
    Ok(listed
        .iter()
        .filter(|s| selected_by_policy(s, &policy_name))
        .cloned()
        .collect())
}

/// `GET /api/v1/snapshots/{namespace}/{name}/retention`
///
/// The whole bucket, not one verdict: which snapshots this policy will keep and
/// which it will prune. Its own route rather than a field on the detail because
/// it is a different question with a different cost — a fan-out policy's plan is
/// every child of the policy, and the detail screen should not pay for it on
/// every open.
async fn retention(
    State(app): State<AppState>,
    CurrentIdentity(id): CurrentIdentity,
    UiPath((namespace, name)): UiPath<(String, String)>,
) -> Result<Json<RetentionPlan>, ApiError> {
    let client = client_for(&app, &id)?;
    let snap = app
        .source
        .get::<Snapshot>(&id, &client, Some(&namespace), &name)
        .await?
        .ok_or_else(|| snapshot_not_found(&namespace, &name))?;

    let policy = resolve_policy(&app, &id, &client, &snap, &namespace)
        .await?
        .ok_or_else(|| no_governing_policy(&snap, &namespace, &name))?;

    // The same population the detail's preview uses, which is the same one the
    // operator's prune uses: the policy's `CONFIG_LABEL` children, in the
    // policy's own namespace.
    let peers = policy_peers(&app, &id, &client, &policy, &namespace, None).await?;
    let policy_ns = policy.metadata.namespace.as_deref().unwrap_or(&namespace);

    // Counted before projecting: the whole point of a plan is that a bucket is
    // shown whole, so it cannot be truncated — but it can be refused. Ahead of
    // the subject's own gates, because a population this size is too large to
    // assemble whichever answer the subject would have earned.
    let candidates = plan_candidate_count(&snap, &policy, &peers);
    if candidates > app.cfg.snapshot_list_cap {
        return Err(plan_too_large(candidates, app.cfg.snapshot_list_cap));
    }

    view_retention_plan(&snap, policy_ns, &policy, &peers, Utc::now())
        .map(Json)
        .ok_or_else(|| not_retention_governed(&snap, &policy, &namespace, &name))
}

/// The 422 a retention plan answers when the policy's population is larger than
/// this deployment will assemble.
///
/// A refusal, never a truncation — and deliberately not `limit`/`offset` over
/// candidates either. A partial bucket misreports which rows are kept: the
/// verdicts in a GFS bucket are decided by the whole bucket competing, so half a
/// bucket is not half an answer, it is a different answer. Showing one would be
/// exactly the lie this endpoint exists to prevent.
///
/// The cap is `KOPIUR_UI_SNAPSHOT_LIST_CAP`, the same knob `/snapshots` already
/// refuses on, so an operator has one number to reason about rather than two.
fn plan_too_large(candidates: usize, cap: usize) -> ApiError {
    problem(
        422,
        "plan-too-large",
        format!(
            "This policy's retention population is {candidates} snapshots, which is more than \
             this kopiur-ui will assemble into one plan (the cap is {cap})."
        ),
        "A retention plan has to show each bucket WHOLE — the verdicts inside one are decided by \
         all of its rows competing, so a truncated bucket would report keeps and prunes that are \
         not what the operator will do. Refusing is the only honest answer that is not a lie.",
        format!(
            "browse the individual rows with /api/v1/snapshots?policy=<name>, which pages — or \
             raise KOPIUR_UI_SNAPSHOT_LIST_CAP above {candidates} if this cluster really does \
             need a plan this large"
        ),
    )
}

/// The 422 for a snapshot no `SnapshotPolicy` governs, or whose policy this
/// caller cannot read.
///
/// Two causes, one status, deliberately different text: "this snapshot has no
/// recipe" and "you cannot see its recipe" send a user to two different places.
fn no_governing_policy(snap: &Snapshot, namespace: &str, name: &str) -> ApiError {
    match snap.spec.policy_ref.as_ref() {
        None => problem(
            422,
            "no-retention-policy",
            format!("Snapshot {namespace}/{name} is not governed by a SnapshotPolicy."),
            "GFS retention is configured on a SnapshotPolicy, and this snapshot names none — a \
             discovered or hand-written snapshot is bounded by the catalog or by whoever made \
             it, not by a retention plan.",
            "look at the repository's catalog settings instead, or set spec.policyRef if this \
             snapshot should be governed by a policy",
        ),
        Some(policy_ref) => problem(
            422,
            "no-retention-policy",
            format!(
                "The SnapshotPolicy {} that governs {namespace}/{name} could not be read.",
                policy_ref.name
            ),
            "The plan is computed from the policy's retention rules, and this request cannot see \
             them — the policy was deleted, or reading SnapshotPolicy in that namespace is not \
             granted to you.",
            "ask for `get` on snapshotpolicies in that namespace, or check whether the policy \
             still exists",
        ),
    }
}

/// The 422 for a snapshot the prune does not evaluate at all.
///
/// Two reasons, deliberately different text, because they send a user to two
/// different places: the row is outside the GFS population (wrong phase, or no
/// controller-written provenance), or it is outside *this policy's* population
/// (the `CONFIG_LABEL` the prune selects on does not name this policy, which is
/// what a snapshot mid-adoption looks like).
fn not_retention_governed(
    snap: &Snapshot,
    policy: &SnapshotPolicy,
    namespace: &str,
    name: &str,
) -> ApiError {
    use kopiur_api::common::PhaseLabel as _;
    let policy_name = policy.metadata.name.clone().unwrap_or_default();
    if retention_view(snap).is_some() && !selected_by_policy(snap, &policy_name) {
        return problem(
            422,
            "not-retention-governed",
            format!(
                "Snapshot {namespace}/{name} names SnapshotPolicy {policy_name}, but the \
                 policy's own retention run does not select it yet."
            ),
            "A prune enumerates its children by the kopiur config label, and this snapshot's \
             label does not name this policy — the shape a discovered snapshot has while it is \
             being adopted, after its spec reference is set and before the operator rewrites \
             the label. Answering with a plan would report it competing for keep slots in a set \
             the operator has never evaluated it in.",
            "wait for the adoption to finish — the label follows within a reconcile — and open \
             the plan again",
        );
    }
    let phase = snap
        .status
        .as_ref()
        .and_then(|s| s.phase.as_ref())
        .map(|p| p.label().to_string())
        .unwrap_or_else(|| "unreconciled".to_string());
    problem(
        422,
        "not-retention-governed",
        format!("Snapshot {namespace}/{name} is {phase}, so GFS retention does not evaluate it."),
        "Only a succeeded snapshot carrying a controller-written kopia manifest competes for a \
         retention slot; a pending, running, failed, deduplicated or deleting row is bounded by \
         something else and has no place in a bucket.",
        "open the retention plan from a succeeded snapshot of the same policy to see which \
         restore points it keeps",
    )
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
    fn a_single_source_policy_previews_one_flat_gfs_bucket() {
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
        assert_eq!(
            kept.reasons,
            vec!["keepDaily slot 1"],
            "the newest of two kept days is slot 1 — the slot is how close it is to ageing out"
        );
        assert!(kept.computed_at.starts_with("2026-05-24T12:00:00"));

        let pruned = view_retention_preview(&d22, &policy, &peers, now).unwrap();
        assert!(
            !pruned.kept,
            "the third-newest day falls outside keepDaily: 2"
        );
        assert!(pruned.reasons.is_empty());
    }

    /// A fan-out child, as a `pvcSelector` policy actually mints one: the
    /// `spec.source` pin is what puts it in its OWN GFS bucket.
    fn fanout_run(pvc: &str, day: u32) -> Arc<Snapshot> {
        snapshot(&format!(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: nightly-{pvc}-{day:02}
  namespace: media
  labels:
    "kopiur.home-operations.com/config": nightly
    "kopiur.home-operations.com/origin": scheduled
spec:
  policyRef: {{ name: nightly }}
  source:
    sourceIndex: 0
    target:
      pvc: {{ namespace: media, name: {pvc} }}
status:
  phase: Succeeded
  origin: scheduled
  timing:
    startTime: "2026-05-{day:02}T02:00:00Z"
    endTime: "2026-05-{day:02}T02:05:00Z"
  snapshot:
    kopiaSnapshotID: k-{pvc}-{day:02}
    identity: {{ username: kopiur, hostname: media, sourcePath: /data }}
"#
        ))
    }

    /// The A-C2 regression guard, and the #346 shape one level up.
    ///
    /// Seven PVCs backed up for seven days under `keepDaily: 7` is 49 protected
    /// restore points: the controller buckets GFS PER SOURCE, so each volume
    /// keeps its own seven days. A preview that ran one flat `select_kept` over
    /// the whole policy's children found seven keepers and reported the other
    /// **42 live, protected snapshots as `kept: false`** — telling the user
    /// their backups were about to be deleted when nothing of the kind was
    /// happening.
    #[test]
    fn a_fanout_policy_keeps_every_source_its_own_days() {
        let policy: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources:
    - pvcSelector: { labelSelector: { matchLabels: { backup: "yes" } } }
  retention: { keepDaily: 7 }
"#,
        );
        let pvcs = [
            "data-0", "data-1", "data-2", "data-3", "data-4", "data-5", "data-6",
        ];
        let days = [18, 19, 20, 21, 22, 23, 24];
        let population: Vec<Arc<Snapshot>> = pvcs
            .iter()
            .flat_map(|pvc| days.iter().map(move |d| fanout_run(pvc, *d)))
            .collect();
        assert_eq!(population.len(), 49);

        let now = DateTime::parse_from_rfc3339("2026-05-24T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let doomed: Vec<&str> = population
            .iter()
            .filter(|s| {
                !view_retention_preview(s, &policy, &population, now)
                    .expect("every succeeded child is previewable")
                    .kept
            })
            .map(|s| s.metadata.name.as_deref().unwrap_or_default())
            .collect();
        assert!(
            doomed.is_empty(),
            "all 49 are inside keepDaily: 7 per source; these were reported as doomed: {doomed:?}"
        );

        // …and the eighth day of ONE volume still falls out of that volume's
        // window, so the bucketing is per-source rather than simply disabled.
        let mut with_older = population.clone();
        let older = fanout_run("data-0", 17);
        with_older.push(older.clone());
        let preview = view_retention_preview(&older, &policy, &with_older, now).unwrap();
        assert!(
            !preview.kept,
            "the eighth day of data-0 is outside its own keepDaily: 7"
        );
        // The other volumes are untouched by data-0 having an extra day.
        let sibling = &population[7]; // data-1's oldest
        assert!(
            view_retention_preview(sibling, &policy, &with_older, now)
                .unwrap()
                .kept,
            "one source's extra day must not evict another source's"
        );
    }

    /// The plan groups the way the prune groups: seven PVCs are seven buckets,
    /// and the snapshot asked about appears in exactly one of them.
    ///
    /// A plan that flattened the population would show every restore point
    /// competing with every other, which is the #346 lie one screen up: 42 of
    /// these 49 would read as "will be pruned".
    #[test]
    fn a_fanout_plan_is_one_bucket_per_source_and_the_subject_sits_in_exactly_one() {
        let policy: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources:
    - pvcSelector: { labelSelector: { matchLabels: { backup: "yes" } } }
  retention: { keepDaily: 7 }
"#,
        );
        let pvcs = [
            "data-0", "data-1", "data-2", "data-3", "data-4", "data-5", "data-6",
        ];
        let days = [18, 19, 20, 21, 22, 23, 24];
        let population: Vec<Arc<Snapshot>> = pvcs
            .iter()
            .flat_map(|pvc| days.iter().map(move |d| fanout_run(pvc, *d)))
            .collect();
        let now = DateTime::parse_from_rfc3339("2026-05-24T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let subject = &population[0]; // data-0, day 18
        let plan = view_retention_plan(subject, "media", &policy, &population, now)
            .expect("a succeeded child of the policy has a plan");

        assert_eq!(
            plan.buckets.len(),
            7,
            "one bucket per backup source, exactly as the prune splits them: {:?}",
            plan.buckets.iter().map(|b| &b.key).collect::<Vec<_>>()
        );
        assert!(!plan.unbounded, "the policy configures keepDaily");
        assert_eq!(plan.policy.name, "nightly");
        assert_eq!(plan.policy.namespace, "media");

        let subject_rows: Vec<&RetentionCandidate> = plan
            .buckets
            .iter()
            .flat_map(|b| b.candidates.iter())
            .filter(|c| c.subject)
            .collect();
        assert_eq!(
            subject_rows.len(),
            1,
            "the snapshot asked about competes in one bucket and appears in one row"
        );
        assert_eq!(subject_rows[0].name, subject.metadata.name.clone().unwrap());
        assert_eq!(subject_rows[0].namespace, "media");

        // Every bucket holds that source's seven days, all kept, newest first.
        for bucket in &plan.buckets {
            assert_eq!(bucket.candidates.len(), 7, "bucket {}", bucket.key);
            assert!(
                bucket.candidates.iter().all(|c| c.kept),
                "keepDaily: 7 keeps all seven days of each source: {}",
                bucket.key
            );
            let times: Vec<&str> = bucket
                .candidates
                .iter()
                .map(|c| c.end_time.as_str())
                .collect();
            let mut newest_first = times.clone();
            newest_first.sort_by(|a, b| b.cmp(a));
            assert_eq!(times, newest_first, "candidates are newest-first");
            assert_eq!(
                bucket.candidates[0].rules,
                vec!["keepDaily slot 1"],
                "the newest of a bucket occupies its rule's first slot"
            );
            assert_eq!(bucket.candidates[6].rules, vec!["keepDaily slot 7"]);
        }
    }

    /// The row that falls out of its own source's window is reported pruned —
    /// with no rules — while its siblings are untouched.
    #[test]
    fn a_plan_marks_the_row_past_the_window_as_pruned_with_no_rules() {
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
        let peers = vec![d24.clone(), d23, d22];
        let now = DateTime::parse_from_rfc3339("2026-05-24T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let plan = view_retention_plan(&d24, "media", &policy, &peers, now).expect("a plan");
        assert_eq!(plan.buckets.len(), 1, "one source, one bucket");
        let candidates = &plan.buckets[0].candidates;
        assert_eq!(
            candidates
                .iter()
                .map(|c| (c.name.as_str(), c.kept, c.rules.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("d24", true, vec!["keepDaily slot 1".to_string()]),
                ("d23", true, vec!["keepDaily slot 2".to_string()]),
                ("d22", false, Vec::new()),
            ],
            "the third-newest day falls outside keepDaily: 2 and carries no rule"
        );
        assert!(candidates.iter().all(|c| !c.pinned));
        assert!(plan.computed_at.starts_with("2026-05-24T12:00:00"));
    }

    /// A policy with no retention prunes nothing, so every candidate is kept and
    /// `unbounded` says why. Running an empty `Retention` through the selection
    /// kernel would literally answer "delete all of them", which is the exact
    /// inversion this branch exists to prevent.
    #[test]
    fn a_policy_with_no_retention_keeps_everything_and_says_it_is_unbounded() {
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
        let d24 = nightly_run("d24", "2026-05-24T02:00:00Z", "Succeeded");
        let d22 = nightly_run("d22", "2026-05-22T02:00:00Z", "Succeeded");
        let peers = vec![d24.clone(), d22];
        let plan = view_retention_plan(&d24, "media", &policy, &peers, Utc::now())
            .expect("an unbounded policy still has a population to show");

        assert!(plan.unbounded);
        let candidates = &plan.buckets[0].candidates;
        assert_eq!(candidates.len(), 2);
        assert!(
            candidates.iter().all(|c| c.kept && c.rules.is_empty()),
            "nothing is pruned by a policy that configures no retention, and no \
             rule holds anything: {candidates:?}"
        );
    }

    /// A pinned row is kept with `pinned` alone, and — crucially — its exemption
    /// does not consume a slot its unpinned competitors would have had.
    #[test]
    fn a_plan_reports_a_pin_as_an_exemption_not_as_a_slot() {
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
  timing: { startTime: "2026-01-01T02:00:00Z", endTime: "2026-01-01T02:00:00Z" }
  snapshot:
    kopiaSnapshotID: k-keepme
    identity: { username: kopiur, hostname: media, sourcePath: /data }
"#,
        );
        let peers = vec![newest.clone(), old_pinned];
        let plan = view_retention_plan(&newest, "media", &policy, &peers, Utc::now()).unwrap();
        let candidates = &plan.buckets[0].candidates;

        // The OLD pinned row: no rule would have kept it, so the pin is doing
        // all the work and is the whole reason.
        let pinned = candidates.iter().find(|c| c.name == "keepme").unwrap();
        assert!(pinned.pinned);
        assert!(pinned.kept);
        assert_eq!(pinned.rules, vec!["pinned"]);

        let newest_row = candidates.iter().find(|c| c.name == "d24").unwrap();
        assert_eq!(
            newest_row.rules,
            vec!["keepDaily slot 1"],
            "a pin does not exempt its holder from the counterfactual, so the \
             slot numbers are the ones the prune's own walk produces"
        );
    }

    /// A pinned row that a rule would ALSO have kept reports BOTH — which is
    /// the useful answer, because it says unpinning would not lose it.
    ///
    /// The sibling test above only covers a pin doing all the work (an old row
    /// no rule keeps), so this is the case the wire doc's "a pin does not mean
    /// exactly one entry" is actually about.
    #[test]
    fn a_pin_that_a_rule_would_also_have_kept_reports_both_reasons() {
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
        // The NEWEST row is the pinned one, so keepDaily would hold it anyway.
        let newest_pinned = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: pinned-newest
  namespace: media
  labels: { "kopiur.home-operations.com/config": nightly }
spec: { policyRef: { name: nightly }, pin: true }
status:
  phase: Succeeded
  origin: manual
  timing: { startTime: "2026-05-25T02:00:00Z", endTime: "2026-05-25T02:00:00Z" }
  snapshot:
    kopiaSnapshotID: k-pinned-newest
    identity: { username: kopiur, hostname: media, sourcePath: /data }
"#,
        );
        let d24 = nightly_run("d24", "2026-05-24T02:00:00Z", "Succeeded");
        let peers = vec![newest_pinned.clone(), d24];
        let plan = view_retention_plan(&newest_pinned, "media", &policy, &peers, Utc::now())
            .expect("a plan");
        let row = plan.buckets[0]
            .candidates
            .iter()
            .find(|c| c.name == "pinned-newest")
            .unwrap();

        assert!(row.pinned);
        assert!(row.kept);
        assert_eq!(
            row.rules,
            vec!["pinned", "keepDaily slot 1"],
            "the pin comes first and carries no slot; the rule that would ALSO \
             have kept it comes after, with its slot"
        );

        // And the preview, which shares the function, says exactly the same.
        let preview = view_retention_preview(&newest_pinned, &policy, &peers, Utc::now()).unwrap();
        assert_eq!(preview.reasons, row.rules);
    }

    /// The population is the PRUNE's — `CONFIG_LABEL`, not `spec.policyRef`.
    ///
    /// The two disagree exactly mid-adoption: a discovered snapshot's spec
    /// reference is set to the adopting policy before the operator rewrites its
    /// label. `policy_of` (which the CLI and the row display correctly use)
    /// answers "nightly"; the operator's own selection — a
    /// `CONFIG_LABEL=nightly` LIST — has never heard of it. Counting it as a
    /// competitor would let it take a keep slot from a row that really is in the
    /// bucket, and give it a verdict from a set it is not in.
    #[test]
    fn the_population_is_the_label_the_prune_selects_on_not_the_spec_reference() {
        // spec.policyRef says `nightly`; the label still says `legacy`.
        let mid_adoption = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: adopting-1
  namespace: media
  labels: { "kopiur.home-operations.com/config": legacy }
spec:
  policyRef: { name: nightly }
status:
  phase: Succeeded
  origin: adopted
  timing: { startTime: "2026-05-24T03:00:00Z", endTime: "2026-05-24T03:05:00Z" }
  snapshot:
    kopiaSnapshotID: k-adopting
    identity: { username: kopiur, hostname: media, sourcePath: /data }
"#,
        );
        assert_eq!(
            policy_of(&mid_adoption),
            Some("nightly"),
            "sanity: the display precedence really does say nightly"
        );
        assert!(
            !selected_by_policy(&mid_adoption, "nightly"),
            "but the prune's label selector does not select it"
        );
        assert!(
            selected_by_policy(&mid_adoption, "legacy"),
            "it is still in the OLD policy's population, which is where the \
             operator will actually evaluate it"
        );

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
        let d24 = nightly_run("d24", "2026-05-24T02:00:00Z", "Succeeded");
        let peers = vec![d24.clone(), mid_adoption.clone()];
        let now = DateTime::parse_from_rfc3339("2026-05-24T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        // The plan for a row that IS selected must not count the mid-adoption
        // row as a competitor.
        let plan = view_retention_plan(&d24, "media", &policy, &peers, now).expect("a plan");
        let names: Vec<&str> = plan
            .buckets
            .iter()
            .flat_map(|b| b.candidates.iter())
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["d24"],
            "only the rows a CONFIG_LABEL=nightly LIST would return"
        );

        // And the mid-adoption row itself gets no plan and no preview under
        // `nightly` — there is no verdict to give about a set it is not in.
        assert!(
            view_retention_plan(&mid_adoption, "media", &policy, &peers, now).is_none(),
            "the subject is gated by the same rule as its peers"
        );
        assert!(view_retention_preview(&mid_adoption, &policy, &peers, now).is_none());

        // The refusal says which of the two reasons applies.
        let refusal = not_retention_governed(&mid_adoption, &policy, "media", "adopting-1");
        assert_eq!(refusal.0.status, 422);
        assert!(
            refusal.0.what.contains("does not select it"),
            "the label case must not be reported as a phase problem: {}",
            refusal.0.what
        );
        assert!(refusal.0.why.contains("config label"), "{}", refusal.0.why);
        assert!(refusal.0.fix.contains("adoption"), "{}", refusal.0.fix);
    }

    /// The plan is refused above the cap rather than truncated: a partial bucket
    /// reports keeps and prunes that are not what the operator will do, which is
    /// the one lie this endpoint exists to prevent.
    #[test]
    fn a_population_larger_than_the_cap_is_refused_rather_than_truncated() {
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
        let population: Vec<Arc<Snapshot>> = (1..=5u32)
            .map(|d| {
                nightly_run(
                    &format!("d{d:02}"),
                    &format!("2026-05-{d:02}T02:00:00Z"),
                    "Succeeded",
                )
            })
            .collect();
        let subject = population[0].clone();

        assert_eq!(
            plan_candidate_count(&subject, &policy, &population),
            5,
            "counted through retention_buckets, so it is the rows that would be \
             rendered rather than the rows that were listed"
        );

        // Both sides of the boundary, on the `> cap` comparison the handler makes.
        let count = plan_candidate_count(&subject, &policy, &population);
        assert!(count <= 5, "at the cap, a plan is still assembled");
        assert!(count > 4, "one above the cap, it is refused");

        let refusal = plan_too_large(count, 4);
        assert_eq!(refusal.0.status, 422);
        assert_eq!(refusal.0.r#type, "urn:kopiur:problem:plan-too-large");
        assert!(refusal.0.what.contains('5'), "{}", refusal.0.what);
        assert!(
            refusal.0.fix.contains("KOPIUR_UI_SNAPSHOT_LIST_CAP"),
            "the remedy names the knob, and it is the SAME knob /snapshots \
             refuses on: {}",
            refusal.0.fix
        );
        assert!(
            refusal.0.fix.contains("/api/v1/snapshots?policy="),
            "and points at the route that CAN page: {}",
            refusal.0.fix
        );
        assert!(
            refusal.0.why.contains("WHOLE"),
            "the why must say why truncation is not on offer: {}",
            refusal.0.why
        );

        // A count at or below the cap really does produce a plan.
        let plan = view_retention_plan(
            &subject,
            "media",
            &policy,
            &population,
            DateTime::parse_from_rfc3339("2026-05-24T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )
        .expect("under the cap, the plan is assembled");
        assert_eq!(plan.buckets[0].candidates.len(), 5);
    }

    /// A row the prune never evaluates has no plan, exactly as it has no
    /// preview. Answering with a plan it does not appear in would read as
    /// "your snapshot is being pruned".
    #[test]
    fn a_row_outside_the_gfs_population_has_no_plan() {
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
        let pending = nightly_run("running-1", "2026-05-24T02:00:00Z", "Running");
        assert!(
            view_retention_plan(
                &pending,
                "media",
                &policy,
                std::slice::from_ref(&pending),
                Utc::now()
            )
            .is_none()
        );
        let problem = not_retention_governed(&pending, &policy, "media", "running-1");
        assert_eq!(problem.0.status, 422);
        assert_eq!(
            problem.0.r#type,
            "urn:kopiur:problem:not-retention-governed"
        );
        assert!(
            problem.0.what.contains("Running"),
            "the refusal names the phase that put it outside: {}",
            problem.0.what
        );
        assert!(!problem.0.why.is_empty());
        assert!(!problem.0.fix.is_empty());
    }

    /// Two causes for "no policy", two remedies. A user whose snapshot has no
    /// recipe and a user who may not read the recipe go to different places.
    #[test]
    fn the_no_policy_refusal_distinguishes_absent_from_unreadable() {
        let discovered = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata: { name: found-1, namespace: media }
spec: {}
status: { phase: Discovered }
"#,
        );
        let absent = no_governing_policy(&discovered, "media", "found-1");
        assert_eq!(absent.0.status, 422);
        assert_eq!(absent.0.r#type, "urn:kopiur:problem:no-retention-policy");
        assert!(absent.0.fix.contains("policyRef"), "{}", absent.0.fix);

        let governed = nightly_run("d24", "2026-05-24T02:00:00Z", "Succeeded");
        let unreadable = no_governing_policy(&governed, "media", "d24");
        assert!(
            unreadable.0.what.contains("nightly"),
            "the refusal names the policy it could not read: {}",
            unreadable.0.what
        );
        assert!(
            unreadable.0.fix.contains("snapshotpolicies"),
            "the remedy names the grant that would fix it: {}",
            unreadable.0.fix
        );
    }

    /// The prune reads `spec.pin`; `status.pinned` is what kopia currently
    /// holds. During an unpin the two disagree, and a preview reading status
    /// would promise a keep the very next prune will not honour.
    #[test]
    fn an_unpinned_snapshot_is_previewed_against_the_spec_not_the_stale_status() {
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
        // spec.pin cleared, kopia not yet caught up.
        let unpinning = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: unpinning
  namespace: media
  labels: { "kopiur.home-operations.com/config": nightly }
spec:
  policyRef: { name: nightly }
  pin: false
status:
  phase: Succeeded
  origin: manual
  pinned: true
  timing: { startTime: "2026-01-01T02:00:00Z", endTime: "2026-01-01T02:00:00Z" }
  snapshot:
    kopiaSnapshotID: k-unpinning
    identity: { username: kopiur, hostname: media, sourcePath: /data }
"#,
        );
        let peers = vec![newest, unpinning.clone()];
        let preview = view_retention_preview(&unpinning, &policy, &peers, Utc::now()).unwrap();
        assert!(
            !preview.kept,
            "the prune reads spec.pin, so the preview must not report a keep status.pinned alone implies"
        );
        assert!(preview.reasons.is_empty());
    }

    /// A `Deleting` row keeps its `status.snapshot`, its `endTime` and its
    /// config label, so a preview with no phase gate lets it claim a keep slot
    /// and shift the buckets under every live row.
    #[test]
    fn a_row_the_prune_ignores_has_no_preview_and_claims_no_slot() {
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
        let deleting = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: going-away
  namespace: media
  labels: { "kopiur.home-operations.com/config": nightly }
spec: { policyRef: { name: nightly } }
status:
  phase: Deleting
  origin: scheduled
  timing: { startTime: "2026-05-25T02:00:00Z", endTime: "2026-05-25T02:00:00Z" }
  snapshot:
    kopiaSnapshotID: k-going-away
    identity: { username: kopiur, hostname: media, sourcePath: /data }
"#,
        );
        let live = nightly_run("d24", "2026-05-24T02:00:00Z", "Succeeded");
        let peers = vec![deleting.clone(), live.clone()];
        let now = DateTime::parse_from_rfc3339("2026-05-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        assert!(
            view_retention_preview(&deleting, &policy, &peers, now).is_none(),
            "a row outside the GFS population has no verdict to give"
        );
        assert!(
            view_retention_preview(&live, &policy, &peers, now)
                .unwrap()
                .kept,
            "and it must not displace the live row it is newer than"
        );
    }

    /// The controller falls back to `creationTimestamp` when `endTime` is
    /// missing; a preview that dropped the candidate would answer `None` for a
    /// snapshot the prune will happily evaluate.
    #[test]
    fn a_snapshot_with_no_end_time_falls_back_to_its_creation_timestamp() {
        let policy: SnapshotPolicy = from_yaml(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: SnapshotPolicy
metadata: { name: nightly, namespace: media }
spec:
  repository: { kind: Repository, name: nas }
  sources: [{ pvc: { name: data } }]
  retention: { keepLatest: 1 }
"#,
        );
        let no_end = snapshot(
            r#"
apiVersion: kopiur.home-operations.com/v1alpha1
kind: Snapshot
metadata:
  name: no-end
  namespace: media
  creationTimestamp: "2026-05-24T02:00:00Z"
  labels: { "kopiur.home-operations.com/config": nightly }
spec: { policyRef: { name: nightly } }
status:
  phase: Succeeded
  origin: scheduled
  snapshot:
    kopiaSnapshotID: k-no-end
    identity: { username: kopiur, hostname: media, sourcePath: /data }
"#,
        );
        let preview =
            view_retention_preview(&no_end, &policy, std::slice::from_ref(&no_end), Utc::now())
                .expect("the prune evaluates this row, so the preview must too");
        assert!(preview.kept);
        assert_eq!(preview.reasons, vec!["keepLatest slot 1"]);
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
        assert_eq!(
            preview.reasons,
            vec!["pinned"],
            "a pin is an exemption from bucketing, so it carries no slot"
        );
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
