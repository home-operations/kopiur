//! Grandfather-father-son (GFS) retention selection (ADR §4.4).
//!
//! `SnapshotPolicy.spec.retention` is the **only** successful-retention driver
//! (SKILL "Retention is GFS-only"). The operator periodically runs this selection
//! over the `Snapshot` CRs for one `(identity, source)` tuple and deletes the CRs
//! that fall outside the kept set; each deleted CR's `deletionPolicy` then governs
//! the snapshot (§4.5). This module is the pure selection kernel — no kube types,
//! no clock — so it's unit-testable with lightweight fakes.
//!
//! ## Algorithm (ADR-0001 §4.4, steps 2–4)
//!
//! 1. Sort candidates by end time, newest first.
//! 2. Apply buckets in order: `keepLatest`, `keepHourly`, `keepDaily`,
//!    `keepWeekly`, `keepMonthly`, `keepAnnual`.
//!    - `keepLatest: N` keeps the N newest backups outright.
//!    - Each time bucket keeps the **most recent** backup within each distinct
//!      period (hour / day / ISO-week / month / year), up to its count `N`,
//!      walking newest→oldest.
//! 3. A backup kept by **any** bucket survives (union). Everything else is deleted.
//!
//! This is deliberately *not* a flat count: a backup that is the newest of its
//! year is held by `keepAnnual` even if hundreds of newer dailies exist — the
//! exact case a flat cap would silently drop (ADR §4.4 "Why not flat-count").
//!
//! ## Empty-policy semantics
//!
//! An all-`None` [`Retention`] selects **no** buckets, so the kept set is empty and
//! every backup is marked for deletion. The caller (controller) is responsible for
//! only invoking GFS when a retention policy is actually configured; this function
//! reports faithfully what the given policy implies. This is documented and tested.

use crate::common::Retention;
use chrono::{DateTime, Datelike, Utc};
use std::collections::BTreeSet;

/// Anything that can stand in for a `Snapshot` during retention selection. Kept tiny
/// so tests use trivial fakes instead of constructing full `Snapshot` CRs.
pub trait SnapshotLike {
    /// The snapshot's completion time — the GFS bucketing key (ADR §4.4 step 2).
    fn end_time(&self) -> DateTime<Utc>;
    /// A stable identifier (kopia snapshot ID or CR name) used in the result sets.
    fn id(&self) -> &str;
    /// Whether this snapshot is pinned (`Snapshot.spec.pin`, ADR-0005 §13(c)). A
    /// pinned snapshot is exempt from GFS retention: [`select_kept`] never places it
    /// in `delete`, regardless of the policy. Defaults `false` so existing impls
    /// (and discovered snapshots) keep their behavior.
    fn pinned(&self) -> bool {
        false
    }
}

/// The outcome of a GFS selection: which ids to keep and which to delete. Both are
/// returned explicitly so callers never have to recompute the complement.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeptSet {
    /// Ids retained by at least one bucket.
    pub keep: Vec<String>,
    /// Ids selected by no bucket — eligible for pruning.
    pub delete: Vec<String>,
}

/// Calendar period a timestamp falls in, used to deduplicate "one per period."
/// Distinct values mean distinct periods; comparing these is how each bucket keeps
/// the newest entry per period.
fn hour_key(t: DateTime<Utc>) -> (i32, u32, u32) {
    (t.year(), t.ordinal(), t.hour())
}
fn day_key(t: DateTime<Utc>) -> (i32, u32) {
    (t.year(), t.ordinal())
}
fn week_key(t: DateTime<Utc>) -> (i32, u32) {
    let iso = t.iso_week();
    (iso.year(), iso.week())
}
fn month_key(t: DateTime<Utc>) -> (i32, u32) {
    (t.year(), t.month())
}
fn year_key(t: DateTime<Utc>) -> i32 {
    t.year()
}

use chrono::Timelike;

/// Walk `sorted` (newest→oldest) and collect the index of the newest entry in each
/// distinct period, stopping once `count` periods have been kept.
fn keep_per_period<K, F>(
    sorted: &[usize],
    times: &[DateTime<Utc>],
    count: usize,
    key: F,
) -> Vec<usize>
where
    K: Ord,
    F: Fn(DateTime<Utc>) -> K,
{
    let mut kept = Vec::new();
    let mut seen: BTreeSet<K> = BTreeSet::new();
    for &idx in sorted {
        if kept.len() >= count {
            break;
        }
        let k = key(times[idx]);
        if seen.insert(k) {
            // First (= newest, since sorted desc) entry in this period.
            kept.push(idx);
        }
    }
    kept
}

/// Select the GFS-kept set from `backups` under `policy` (ADR §4.4).
///
/// Returns a [`KeptSet`] partitioning every input id into `keep`/`delete`. Input
/// order is irrelevant; `keep` is returned newest-first, `delete` newest-first too.
/// Ties on `end_time` are broken by id for determinism.
///
/// ```
/// use chrono::{DateTime, TimeZone, Utc};
/// use kopiur_api::{select_kept, SnapshotLike};
/// use kopiur_api::common::Retention;
///
/// // A trivial fake honoring SnapshotLike — no kube CRs needed for selection.
/// struct Snap { id: String, end: DateTime<Utc> }
/// impl SnapshotLike for Snap {
///     fn end_time(&self) -> DateTime<Utc> { self.end }
///     fn id(&self) -> &str { &self.id }
/// }
/// let day = |d: u32| Utc.with_ymd_and_hms(2026, 5, d, 2, 0, 0).single().unwrap();
/// let snaps = vec![
///     Snap { id: "d24".into(), end: day(24) },
///     Snap { id: "d23".into(), end: day(23) },
///     Snap { id: "d22".into(), end: day(22) },
/// ];
///
/// // keepDaily: 2 — keep the newest per day for the 2 newest days; prune the rest.
/// let policy: Retention =
///     serde_json::from_value(serde_json::json!({ "keepDaily": 2 })).unwrap();
/// let kept = select_kept(&snaps, &policy);
/// assert_eq!(kept.keep, vec!["d24", "d23"]); // newest-first
/// assert_eq!(kept.delete, vec!["d22"]);
/// ```
pub fn select_kept<T: SnapshotLike>(backups: &[T], policy: &Retention) -> KeptSet {
    if backups.is_empty() {
        return KeptSet::default();
    }

    let times: Vec<DateTime<Utc>> = backups.iter().map(|b| b.end_time()).collect();

    // Indices sorted by end_time descending; id as a deterministic tiebreaker.
    let mut order: Vec<usize> = (0..backups.len()).collect();
    order.sort_by(|&a, &b| {
        times[b]
            .cmp(&times[a])
            .then_with(|| backups[a].id().cmp(backups[b].id()))
    });

    let mut keep_idx: BTreeSet<usize> = BTreeSet::new();

    // keepLatest: the N newest outright.
    if let Some(n) = policy.keep_latest {
        for &idx in order.iter().take(n as usize) {
            keep_idx.insert(idx);
        }
    }
    if let Some(n) = policy.keep_hourly {
        keep_idx.extend(keep_per_period(&order, &times, n as usize, hour_key));
    }
    if let Some(n) = policy.keep_daily {
        keep_idx.extend(keep_per_period(&order, &times, n as usize, day_key));
    }
    if let Some(n) = policy.keep_weekly {
        keep_idx.extend(keep_per_period(&order, &times, n as usize, week_key));
    }
    if let Some(n) = policy.keep_monthly {
        keep_idx.extend(keep_per_period(&order, &times, n as usize, month_key));
    }
    if let Some(n) = policy.keep_annual {
        keep_idx.extend(keep_per_period(&order, &times, n as usize, year_key));
    }

    let mut keep = Vec::new();
    let mut delete = Vec::new();
    for &idx in &order {
        // A pinned snapshot is exempt from GFS retention (ADR-0005 §13(c)): it always
        // survives a prune even when no bucket selected it — kopia would also refuse to
        // expire it. Kept newest-first alongside bucket-kept ids.
        if keep_idx.contains(&idx) || backups[idx].pinned() {
            keep.push(backups[idx].id().to_string());
        } else {
            delete.push(backups[idx].id().to_string());
        }
    }
    KeptSet { keep, delete }
}

// --- the population, and how it is bucketed ---------------------------------

/// A minimal view of a `Snapshot` for retention selection: its CR name (the id
/// used in delete decisions) and its snapshot end time (the GFS bucketing key).
///
/// `Clone` so the adoption retention gate (adoption inv. 8) can union these
/// views with candidate views without re-deriving them from the CRs.
#[derive(Debug, Clone)]
pub struct SnapshotRetentionView {
    /// CR name — the stable id returned in the kept/delete sets.
    pub name: String,
    /// Snapshot completion time (from `status.snapshot`/`status.timing`).
    pub end_time: DateTime<Utc>,
    /// Whether the `Snapshot` is pinned (`spec.pin`, ADR-0005 §13(c)) — exempt
    /// from GFS retention (never selected for deletion).
    pub pinned: bool,
}

impl SnapshotLike for SnapshotRetentionView {
    fn end_time(&self) -> DateTime<Utc> {
        self.end_time
    }
    fn id(&self) -> &str {
        &self.name
    }
    fn pinned(&self) -> bool {
        self.pinned
    }
}

/// Build a retention view from a `Snapshot` CR, using `status.timing.endTime`
/// (falling back to the CR creation timestamp). Returns `None` if the backup is
/// not in a terminal successful state — only successful snapshots participate in
/// GFS (failures are bounded separately by `failedJobsHistoryLimit`).
///
/// Lives in `kopiur-api` rather than in the controller because the web UI's
/// "would retention keep this?" preview has to select the same population the
/// prune does. A preview built from a different population is not a preview: one
/// that admitted `Deleting` rows, or dropped rows whose `endTime` was missing,
/// answers about a set the operator will never evaluate.
pub fn retention_view(b: &crate::Snapshot) -> Option<SnapshotRetentionView> {
    use crate::SnapshotPhase;
    let status = b.status.as_ref()?;
    // Exhaustive, not `!= Succeeded`: GFS membership is a CLASSIFICATION whose
    // "no" side spans four unrelated meanings (in-flight, failed, deduped,
    // foreign). A new phase silently defaulting to "not retention-governed"
    // would quietly stop protecting a real restore point, so the compiler asks
    // here first. Deliberately NOT `is_terminal()`: `Discovered`/`Unchanged`
    // are terminal but must not claim a GFS bucket.
    let participates_in_gfs = status.phase.as_ref().is_some_and(|p| match p {
        SnapshotPhase::Succeeded => true,
        // `Unchanged` owns no manifest, so it must never displace one that
        // exists; `Discovered` is bounded by the catalog, not by this policy's
        // retention; the rest are not terminal successes at all.
        SnapshotPhase::Unchanged
        | SnapshotPhase::Discovered
        | SnapshotPhase::Pending
        | SnapshotPhase::Running
        | SnapshotPhase::Failed
        | SnapshotPhase::Deleting => false,
        // Never let a phase this build cannot read enter a set whose losers get
        // DELETED from the repository.
        SnapshotPhase::Unknown(_) => false,
    });
    if !participates_in_gfs {
        return None;
    }
    // PROVENANCE (defense in depth): a `Succeeded` row only participates in GFS
    // when it carries CONTROLLER-WRITTEN provenance (`status.snapshot`, the kopia
    // id the operator produced or adopted). This closes the phantom-Succeeded
    // displacement even if a phase were ever pinned without one — a forged bare
    // `origin: adopted` label whose creationTimestamp fallback would otherwise
    // claim a GFS bucket and displace a real snapshot into the retention delete
    // set. Every genuine produced/adopted row has `status.snapshot`.
    status.snapshot.as_ref()?;
    let end_time = status
        .timing
        .as_ref()
        .and_then(|t| t.end_time.as_deref())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            // metadata.creationTimestamp is a k8s-openapi `Time` wrapping a
            // jiff `Timestamp`; convert via unix seconds to chrono.
            b.metadata
                .creation_timestamp
                .as_ref()
                .and_then(|t| DateTime::<Utc>::from_timestamp(t.0.as_second(), 0))
        })?;
    Some(SnapshotRetentionView {
        name: b.metadata.name.clone().unwrap_or_default(),
        end_time,
        // A pinned Snapshot is exempt from GFS pruning (ADR-0005 §13(c)).
        // `spec.pin` is what the user asked for and what the prune reads;
        // `status.pinned` is what kopia currently holds, and during an unpin the
        // two disagree — reading status there would promise a keep the next
        // prune will not honor.
        pinned: b.spec.pin,
    })
}

/// **Pure.** The retention bucket a `Snapshot` belongs to.
///
/// One bucket per distinct backup source, so GFS keeps `keepDaily` days *of
/// each PVC* rather than `keepDaily` snapshots across all of them. Empty for an
/// un-fanned Snapshot, which is what makes a single-source policy one bucket
/// and therefore unchanged.
///
/// **Multi-repo fan-out (#368):** while the policy is CURRENTLY multi-repo
/// (`policy_is_multi`), the key also carries the child's mint-time repository
/// pin (`spec.repository`, normalized at mint), so GFS keeps `keepDaily` days
/// per (source, repository) — the N repositories are independent captures and
/// must retain independently. The repo component comes from the SPEC pin ONLY
/// (never `status.resolved` — a status-derived key would flap with backfills),
/// and applies ONLY while the policy is multi-repo:
///
/// - single-repo policy (including after a multi→single edit): source-only
///   buckets, byte-identical to today. Old pinned children merge back into the
///   flat buckets — a documented TRANSIENT GFS mixing (the surviving repo's
///   rows and the removed repo's leftovers compete in one bucket) that
///   self-resolves as the removed repo's rows age out of every keep window.
/// - multi-repo policy: (source, pin) buckets. Rows with NO pin (pre-feature
///   children minted before the single→multi edit) land in the ""-repo bucket
///   and age out; the policy reconciler's spec-pin backfill converges them into
///   their real buckets first, so the ""-bucket is a shrinking transition set,
///   not a steady state.
pub fn retention_group_key(b: &crate::Snapshot, policy_is_multi: bool) -> String {
    let source = match b.spec.source.as_ref().map(|s| &s.target) {
        Some(crate::SnapshotSourceTarget::Pvc(t)) => {
            format!("pvc/{}/{}", t.namespace, t.name)
        }
        None => String::new(),
    };
    if !policy_is_multi {
        return source;
    }
    let repo = b
        .spec
        .repository
        .as_ref()
        .map(|r| crate::common::repo_key(r, b.metadata.namespace.as_deref().unwrap_or_default()))
        .unwrap_or_default();
    // '\n' can appear in neither component (DNS names / repo keys), so the
    // joined key is injective over (source, repo).
    format!("{source}\n{repo}")
}

/// **Pure.** The GFS population of `backups`, bucketed exactly as a prune would
/// bucket it.
///
/// This is the one place the *shape* of a retention run is decided:
/// [`retention_view`] filters the population and `retention_group_key` splits
/// it. `backups_to_delete` runs `select_kept` over these buckets and returns the
/// losers; the web UI runs the same `select_kept` over the same buckets and asks
/// whether one id is a winner. Sharing this means a preview cannot answer about
/// a set the operator will never evaluate.
///
/// Terminating rows are excluded (#382 C1): with the population served from a
/// reflector store, an externally-deleted-but-still-cached NEWER snapshot must
/// not claim a keep slot and push a LIVE row into the delete set.
pub fn retention_buckets(
    backups: &[&crate::Snapshot],
    policy_is_multi: bool,
) -> std::collections::BTreeMap<String, Vec<SnapshotRetentionView>> {
    let mut buckets: std::collections::BTreeMap<String, Vec<SnapshotRetentionView>> =
        std::collections::BTreeMap::new();
    for b in backups {
        if b.metadata.deletion_timestamp.is_some() {
            continue;
        }
        if let Some(v) = retention_view(b) {
            buckets
                .entry(retention_group_key(b, policy_is_multi))
                .or_default()
                .push(v);
        }
    }
    buckets
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Minimal fake honoring `SnapshotLike` — no kube CRs in retention tests.
    struct Fake {
        id: String,
        end: DateTime<Utc>,
        pinned: bool,
    }
    impl SnapshotLike for Fake {
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

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).single().unwrap()
    }
    fn fake(id: &str, t: DateTime<Utc>) -> Fake {
        Fake {
            id: id.into(),
            end: t,
            pinned: false,
        }
    }
    fn pinned(id: &str, t: DateTime<Utc>) -> Fake {
        Fake {
            id: id.into(),
            end: t,
            pinned: true,
        }
    }

    fn policy(
        latest: Option<u32>,
        hourly: Option<u32>,
        daily: Option<u32>,
        weekly: Option<u32>,
        monthly: Option<u32>,
        annual: Option<u32>,
    ) -> Retention {
        Retention {
            keep_latest: latest,
            keep_hourly: hourly,
            keep_daily: daily,
            keep_weekly: weekly,
            keep_monthly: monthly,
            keep_annual: annual,
        }
    }

    fn as_set(v: &[String]) -> BTreeSet<&str> {
        v.iter().map(String::as_str).collect()
    }

    #[test]
    fn empty_input_yields_empty_sets() {
        let got = select_kept::<Fake>(&[], &policy(Some(5), None, None, None, None, None));
        assert!(got.keep.is_empty());
        assert!(got.delete.is_empty());
    }

    #[test]
    fn empty_policy_keeps_nothing() {
        // All-None policy → no buckets selected → everything deleted.
        let backups = vec![
            fake("a", at(2026, 5, 24, 2, 0)),
            fake("b", at(2026, 5, 23, 2, 0)),
        ];
        let got = select_kept(&backups, &Retention::default());
        assert!(got.keep.is_empty(), "empty policy keeps nothing");
        assert_eq!(as_set(&got.delete), ["a", "b"].into_iter().collect());
    }

    #[test]
    fn keep_latest_keeps_n_newest() {
        let backups = vec![
            fake("d1", at(2026, 5, 24, 2, 0)),
            fake("d2", at(2026, 5, 23, 2, 0)),
            fake("d3", at(2026, 5, 22, 2, 0)),
            fake("d4", at(2026, 5, 21, 2, 0)),
        ];
        let got = select_kept(&backups, &policy(Some(2), None, None, None, None, None));
        assert_eq!(as_set(&got.keep), ["d1", "d2"].into_iter().collect());
        assert_eq!(as_set(&got.delete), ["d3", "d4"].into_iter().collect());
    }

    #[test]
    fn keep_daily_keeps_one_newest_per_day() {
        // Three backups on day 24 (keep the 02:00 one), one each on 23 and 22.
        let backups = vec![
            fake("a", at(2026, 5, 24, 0, 5)),
            fake("b", at(2026, 5, 24, 1, 30)),
            fake("c", at(2026, 5, 24, 2, 0)), // newest on the 24th
            fake("d", at(2026, 5, 23, 2, 0)),
            fake("e", at(2026, 5, 22, 2, 0)),
        ];
        let got = select_kept(&backups, &policy(None, None, Some(14), None, None, None));
        // One per distinct day, newest within the day.
        assert_eq!(as_set(&got.keep), ["c", "d", "e"].into_iter().collect());
        assert_eq!(as_set(&got.delete), ["a", "b"].into_iter().collect());
    }

    #[test]
    fn keep_daily_count_caps_number_of_days() {
        let backups = vec![
            fake("d24", at(2026, 5, 24, 2, 0)),
            fake("d23", at(2026, 5, 23, 2, 0)),
            fake("d22", at(2026, 5, 22, 2, 0)),
            fake("d21", at(2026, 5, 21, 2, 0)),
        ];
        let got = select_kept(&backups, &policy(None, None, Some(2), None, None, None));
        // Only the 2 newest days kept.
        assert_eq!(as_set(&got.keep), ["d24", "d23"].into_iter().collect());
        assert_eq!(as_set(&got.delete), ["d22", "d21"].into_iter().collect());
    }

    #[test]
    fn keep_latest_unions_with_keep_daily() {
        // Two backups same day: keepDaily keeps the newest (c), keepLatest:2 also
        // pulls in the second-newest overall (b) even though it shares c's day.
        let backups = vec![
            fake("c", at(2026, 5, 24, 6, 0)),
            fake("b", at(2026, 5, 24, 5, 0)),
            fake("a", at(2026, 5, 23, 5, 0)),
        ];
        let got = select_kept(&backups, &policy(Some(2), None, Some(7), None, None, None));
        // c kept by both; b kept by keepLatest; a kept by keepDaily (day 23).
        assert_eq!(as_set(&got.keep), ["a", "b", "c"].into_iter().collect());
        assert!(got.delete.is_empty());
    }

    #[test]
    fn annual_snapshot_survives_flood_of_newer_dailies() {
        // The §4.4 "why not flat-count" case. One old end-of-2024 snapshot plus a
        // pile of 2026 dailies. keepDaily:3 + keepAnnual:2 must retain the 2024
        // snapshot as the newest-of-its-year even though it's far down the list.
        let mut backups = vec![fake("y2024", at(2024, 12, 31, 23, 0))];
        for d in 1..=10u32 {
            backups.push(fake(&format!("y2026-{d:02}"), at(2026, 5, d, 2, 0)));
        }
        // Newest 2026 daily is day 10; year 2026's representative is day 10,
        // year 2024's representative is y2024.
        let got = select_kept(&backups, &policy(None, None, Some(3), None, None, Some(2)));
        let keep = as_set(&got.keep);
        assert!(
            keep.contains("y2024"),
            "annual snapshot must not be dropped by daily flood; kept={keep:?}"
        );
        // keepDaily:3 keeps the 3 newest days of 2026.
        assert!(keep.contains("y2026-10"));
        assert!(keep.contains("y2026-09"));
        assert!(keep.contains("y2026-08"));
        // Older 2026 dailies not covered by any bucket are deleted.
        assert!(got.delete.contains(&"y2026-01".to_string()));
    }

    #[test]
    fn monthly_and_weekly_pick_newest_in_period() {
        let backups = vec![
            fake("may-late", at(2026, 5, 28, 2, 0)),
            fake("may-early", at(2026, 5, 2, 2, 0)),
            fake("apr", at(2026, 4, 15, 2, 0)),
            fake("mar", at(2026, 3, 15, 2, 0)),
        ];
        let got = select_kept(&backups, &policy(None, None, None, None, Some(2), None));
        // keepMonthly:2 → newest of May (may-late) and newest of April (apr).
        assert_eq!(as_set(&got.keep), ["may-late", "apr"].into_iter().collect());
    }

    #[test]
    fn pinned_snapshot_survives_a_prune_that_would_delete_it() {
        // ADR-0005 §13(c): a pinned snapshot is exempt from GFS retention. With
        // keepLatest:1, the two older snapshots would normally be deleted — but the
        // pinned one must survive while the unpinned one is pruned.
        let backups = vec![
            fake("newest", at(2026, 5, 24, 2, 0)),
            pinned("pinned-old", at(2026, 5, 20, 2, 0)),
            fake("unpinned-old", at(2026, 5, 19, 2, 0)),
        ];
        let got = select_kept(&backups, &policy(Some(1), None, None, None, None, None));
        let keep = as_set(&got.keep);
        let del = as_set(&got.delete);
        assert!(keep.contains("newest"), "keepLatest:1 keeps the newest");
        assert!(
            keep.contains("pinned-old"),
            "a pinned snapshot must survive a prune that would otherwise delete it"
        );
        assert!(
            del.contains("unpinned-old"),
            "the unpinned older snapshot is pruned"
        );
        assert!(!del.contains("pinned-old"), "pinned is never in delete");
    }

    #[test]
    fn every_backup_kept_by_any_bucket_survives() {
        // Mixed policy; assert the kept set is exactly the union and no kept id
        // appears in delete.
        let backups = vec![
            fake("now", at(2026, 5, 24, 12, 0)),
            fake("earlier-today", at(2026, 5, 24, 1, 0)),
            fake("yesterday", at(2026, 5, 23, 1, 0)),
            fake("last-week", at(2026, 5, 16, 1, 0)),
        ];
        let got = select_kept(
            &backups,
            &policy(Some(1), None, Some(2), Some(2), None, None),
        );
        let keep = as_set(&got.keep);
        let del = as_set(&got.delete);
        for id in keep.iter() {
            assert!(!del.contains(id), "id {id} in both keep and delete");
        }
        // Every input is accounted for exactly once.
        assert_eq!(keep.len() + del.len(), 4);
    }

    #[test]
    fn e2e_gfs_history_partitions_exactly_as_the_retention_e2e_expects() {
        // The EXACT history + spec the `gfs_time_buckets_prune_backdated_history`
        // e2e (crates/e2e/tests/retention.rs) seeds, pinned hermetically so the
        // e2e's in-test expectation can never drift from the kernel. Note
        // keepAnnual: 2 — buckets are the N most recent periods CONTAINING
        // snapshots, so holding a prior-year snapshot needs the 2026 bucket
        // (the newest snapshot's year) plus one more.
        let backups = vec![
            fake("e2e-gfs-1", at(2025, 4, 10, 10, 0)),
            fake("e2e-gfs-2", at(2026, 3, 2, 10, 0)),
            fake("e2e-gfs-3", at(2026, 4, 6, 10, 0)),
            fake("e2e-gfs-4", at(2026, 5, 25, 10, 0)),
            fake("e2e-gfs-5", at(2026, 6, 1, 10, 0)),
            fake("e2e-gfs-6", at(2026, 6, 8, 10, 0)),
            fake("e2e-gfs-7", at(2026, 6, 8, 11, 0)),
        ];
        let policy: Retention = serde_json::from_value(serde_json::json!({
            "keepLatest": 1, "keepDaily": 2, "keepWeekly": 2,
            "keepMonthly": 2, "keepAnnual": 2
        }))
        .unwrap();
        let got = select_kept(&backups, &policy);
        assert_eq!(
            as_set(&got.keep),
            ["e2e-gfs-1", "e2e-gfs-4", "e2e-gfs-5", "e2e-gfs-7"]
                .into_iter()
                .collect(),
            "keep: latest+daily+weekly (gfs-7/5), monthly #2 (gfs-4), annual #2 (gfs-1)"
        );
        assert_eq!(
            as_set(&got.delete),
            ["e2e-gfs-2", "e2e-gfs-3", "e2e-gfs-6"]
                .into_iter()
                .collect(),
            "delete: months outside keepMonthly:2 and the same-day older duplicate"
        );
    }

    /// Convergence keystone for adoption invariant 8 (controller crate): the
    /// selection is stable on its own kept set — re-running it over exactly the
    /// survivors deletes nothing and keeps the same set. This is what makes the
    /// adoption gate's pre-prune evaluation equal the next retention pass's
    /// decision: pruning the non-kept rows never re-selects (or de-selects) a
    /// survivor. Holds because the selection is time-invariant (buckets derive
    /// purely from end times), a non-kept row is never a newest-in-period
    /// representative, and ties break deterministically by id.
    #[test]
    fn select_kept_is_stable_on_its_own_kept_set() {
        let populations: Vec<Vec<Fake>> = vec![
            // Dense multi-day spread with a same-instant id tie and a pinned
            // straggler far outside every bucket.
            vec![
                fake("tie-a", at(2026, 5, 24, 2, 0)),
                fake("tie-b", at(2026, 5, 24, 2, 0)),
                fake("d23", at(2026, 5, 23, 2, 0)),
                fake("d22-am", at(2026, 5, 22, 2, 0)),
                fake("d22-pm", at(2026, 5, 22, 14, 0)),
                fake("w-old", at(2026, 5, 1, 2, 0)),
                pinned("pin-ancient", at(2020, 1, 1, 0, 0)),
            ],
            // Single row; empty input handled by select_kept directly.
            vec![fake("only", at(2026, 5, 24, 2, 0))],
        ];
        let policies = [
            policy(Some(2), None, None, None, None, None),
            policy(None, None, Some(2), None, None, None),
            policy(Some(1), None, Some(2), Some(1), Some(1), Some(1)),
            policy(None, None, None, None, None, None), // keeps only pins
        ];
        for snaps in &populations {
            for pol in &policies {
                let first = select_kept(snaps, pol);
                let survivors: Vec<Fake> = snaps
                    .iter()
                    .filter(|s| first.keep.iter().any(|k| k == &s.id))
                    .map(|s| Fake {
                        id: s.id.clone(),
                        end: s.end,
                        pinned: s.pinned,
                    })
                    .collect();
                let second = select_kept(&survivors, pol);
                assert!(
                    second.delete.is_empty(),
                    "keep(S) must be a fixed point; policy {pol:?} re-deleted {:?}",
                    second.delete
                );
                assert_eq!(
                    as_set(&second.keep),
                    as_set(&first.keep),
                    "keep(keep(S)) == keep(S) for policy {pol:?}"
                );
            }
        }
    }
}
