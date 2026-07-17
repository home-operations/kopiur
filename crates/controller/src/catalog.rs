//! Discovered-snapshot catalog: materialize (and expire) `origin: discovered`
//! `Snapshot` CRs from a repository's kopia snapshot listing (ADR §2.1/§2.3,
//! §3.1 `catalog`).
//!
//! Shared by the `Repository` and `ClusterRepository` reconcilers so the rules
//! cannot fork between the two kinds. The catalog **decision** is a pure
//! function ([`plan_catalog`]) and is unit-tested exhaustively here; the kube
//! LIST/create/delete and the cross-namespace placement lookups are the thin IO
//! parts ([`scan`]).
//!
//! ## The rules (all enforced by [`plan_catalog`])
//!
//! - **Dedup** by `(repository CR UID, kopiaSnapshotID)`: two scans never
//!   materialize the same snapshot twice, and the same kopia snapshot under a
//!   *different* repository CR is a distinct row (that's how adopting a
//!   repository materializes snapshots another `Repository` produced).
//! - **Produced snapshots are not "discovered".** A snapshot whose id is already
//!   carried by a scheduled/manual `Snapshot` CR resolving to *this* repository
//!   never gets a discovered row — `discovered` means "found in the repository,
//!   not produced through this CR" (a rescan must not duplicate this cluster's
//!   own backups).
//! - **Bounds** (`spec.catalog.retain`): the most-recent `perIdentity` rows per
//!   `username@hostname:path`, nothing older than `maxAgeDays`. Rows beyond the
//!   bounds are **expired — the CR is deleted, the kopia snapshot is untouched**
//!   (discovered rows are forced `deletionPolicy: Retain`, §4.5).
//! - **Absence expiry**: a row whose snapshot no longer appears in a *complete*
//!   listing was deleted repository-side (an external writer pruned it) — the
//!   stale row is expired. Skipped when the listing was truncated (absence is
//!   unknowable from a partial list).
//!
//! ## Identity-aware placement (multi-cluster shared repo)
//!
//! When a repository's `identityDefaults.cluster` is set (M1: [`kopiur_api::classify_hostname`]/
//! [`kopiur_api::HostClass`]), a scan runs ONE placement pass over the FULL listing
//! BEFORE [`plan_catalog`] ([`plan_placements`]): every distinct hostname is classified
//! `Bare`/`OwnCluster`/`ForeignCluster` and decided [`Place`](PlacementDecision::Place)/
//! [`Unplaced`](PlacementDecision::Unplaced)/[`ForeignIgnored`](PlacementDecision::ForeignIgnored)
//! ([`decide_cluster_placement`]/[`decide_namespace_placement`], pure). Entries decided
//! `ForeignIgnored` are fed to `plan_catalog` exactly like `produced_ids` — filtered in
//! the same `eligible` chain spot, so they never consume a `retain.perIdentity` slot,
//! and any PRE-EXISTING discovered row for one expires via the ordinary absence
//! mechanics once it's no longer eligible. Without a cluster identity, every hostname
//! classifies `Bare` and this pass is byte-identical to the pre-M4 per-entry placement
//! (proven by fixture tests). `spec.catalog.foreignSnapshots` (M3:
//! [`ForeignSnapshots`]/[`CatalogBounds::effective_foreign_snapshots`]) decides what
//! happens to a `ForeignCluster`-classified entry: `Ignore` (default) drops it,
//! `Fallback` materializes it into `catalog.fallbackNamespace` like any other entry.
//! The mover's `BootstrapRepository` op additionally pre-filters `ForeignCluster`
//! entries BEFORE its own `MAX_RETURNED_SNAPSHOTS` cap when cluster mode is on and the
//! effective policy is `Ignore` (`catalog_foreign_prefilter_cluster`); this pass still
//! sees (and must still ignore) any **bare**-hostname foreign entries, which the mover
//! cannot pre-filter (classifying them needs a namespace lookup only the controller
//! can do).
//!
//! ## Refresh cadence
//!
//! An **initial** scan always runs — on first bootstrap and again on any spec
//! change (the `generation != observedGeneration` arm of [`scan_due`] /
//! [`bootstrap_recycle_due`]). **Repeated, timed** re-scans are **opt-in** via
//! `spec.catalog.periodicRefresh` (off by default): when enabled, a scan runs once
//! `status.catalog.lastRefreshAt` is older than the effective
//! `spec.catalog.refreshInterval` (default
//! [`kopiur_api::consts::DEFAULT_CATALOG_REFRESH_INTERVAL`]) — see [`refresh_due`].
//! With it off, a succeeded repository bootstraps once and is never recycled on a
//! timer. Gating the scan also gates the `lastRefreshAt` status write, so a Ready
//! repository's status is byte-stable between refreshes (the status-churn rule).
//! Object-store repositories re-list by recycling their finished bootstrap Job
//! ([`bootstrap_recycle_due`]); bare-path filesystem repositories re-list in-process.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::ResourceExt;
use kube::api::{Api, DeleteParams, ListParams};

use kopiur_api::cluster_repository::AllowedNamespaces;
use kopiur_api::common::{CatalogBounds, CatalogRetain, ForeignSnapshots, RepositoryKind};
use kopiur_api::snapshot::repository_ref_for;
use kopiur_api::{HostClass, Snapshot, classify_hostname, validate};
use kopiur_kopia::{SnapshotListEntry, SnapshotSource};

use crate::consts::{ORIGIN_LABEL, REPOSITORY_UID_LABEL, SNAPSHOT_ID_LABEL};
use crate::context::Context;
use crate::error::{Error, Result};
use crate::io;

/// The dedup key for a discovered snapshot: `(Repository CR UID, kopiaSnapshotID)`
/// (ADR §2.1).
pub fn catalog_dedup_key(repo_uid: &str, snapshot_id: &str) -> (String, String) {
    (repo_uid.to_string(), snapshot_id.to_string())
}

/// The kopia identity a snapshot was taken under, as the canonical
/// `username@hostname:path` string `catalog.retain.perIdentity` groups by.
pub fn identity_key(source: &SnapshotSource) -> String {
    format!("{}@{}:{}", source.user_name, source.host, source.path)
}

/// `true` when a catalog scan is due: never scanned, an unparseable stamp
/// (defensive — we wrote it), or `last_refresh_at + interval <= now`.
pub fn refresh_due(
    last_refresh_at: Option<&str>,
    interval: std::time::Duration,
    now: DateTime<Utc>,
) -> bool {
    let Some(raw) = last_refresh_at else {
        return true;
    };
    let Ok(last) = DateTime::parse_from_rfc3339(raw) else {
        return true;
    };
    let Ok(interval) = chrono::Duration::from_std(interval) else {
        return true;
    };
    last.with_timezone(&Utc) + interval <= now
}

/// Whether a fresh reverify request should force a re-probe now, bypassing the
/// refresh timer. Gated on the repo being `Ready` and the token differing from the
/// last one honored (`status.lastReverifyAt`) — that comparison is the loop guard.
pub fn reverify_due(token: Option<&str>, honored: Option<&str>, phase_ready: bool) -> bool {
    phase_ready && token.is_some() && token != honored
}

/// Whether a `Snapshot` should (re)write the reverify-request annotation. Rate
/// limited via the existing timestamp (shared across `Snapshot`s) so a wave of
/// failures forces at most one re-probe per `min_interval`. Absent/unparseable ⇒ yes.
pub fn should_request_reverify(
    existing: Option<&str>,
    now: DateTime<Utc>,
    min_interval: std::time::Duration,
) -> bool {
    let Some(raw) = existing else {
        return true;
    };
    let Ok(last) = DateTime::parse_from_rfc3339(raw) else {
        return true;
    };
    let Ok(min_interval) = chrono::Duration::from_std(min_interval) else {
        return true;
    };
    last.with_timezone(&Utc) + min_interval <= now
}

/// The steady-state requeue for a Ready repository: the usual 5 minutes, or — only
/// when `periodicRefresh` is on — the catalog refresh interval when the user asked
/// for a faster re-scan cadence (otherwise a sub-5m `refreshInterval` would silently
/// never fire on time). With periodic refresh off, the interval is inert, so we stay
/// at the 5-minute liveness cadence.
pub fn reconcile_interval(catalog: Option<&CatalogBounds>) -> std::time::Duration {
    let base = std::time::Duration::from_secs(300);
    if CatalogBounds::periodic_refresh_enabled(catalog) {
        base.min(CatalogBounds::effective_refresh_interval(catalog))
    } else {
        base
    }
}

/// `true` when a *finished* bootstrap Job should be deleted so the next
/// reconcile re-runs it with a fresh `snapshot list`: the repository is `Ready`
/// and either the catalog refresh is due or the spec changed since the result
/// was taken (`generation != observedGeneration` — a re-pointed backend must
/// re-bootstrap, not keep reporting the old repository's identity).
pub fn bootstrap_recycle_due(
    phase_is_ready: bool,
    generation: Option<i64>,
    observed_generation: Option<i64>,
    last_refresh_at: Option<&str>,
    interval: std::time::Duration,
    periodic_enabled: bool,
    now: DateTime<Utc>,
) -> bool {
    if !phase_is_ready {
        return false;
    }
    if generation != observed_generation {
        return true;
    }
    // The timed refresh arm only fires when periodic refresh is opted in; otherwise a
    // succeeded bootstrap is never recycled on a timer (one-time bootstrap semantics).
    periodic_enabled && refresh_due(last_refresh_at, interval, now)
}

/// `true` when a fresh repository listing should actually be SCANNED into the
/// catalog (materialize/expire discovered rows): the timed refresh is due, OR
/// the spec changed since the last reconciled generation. The generation arm is
/// load-bearing for `catalog.retain` edits: a tightened `perIdentity` recycles
/// the bootstrap Job for a fresh listing ([`bootstrap_recycle_due`]'s own
/// generation arm), but gating the scan on `refresh_due` alone then threw that
/// fresh result away — the over-cap rows only expired at the NEXT timed
/// refresh (up to `refreshInterval` later), not on the spec change that asked
/// for it. The caller passes the PRE-reconcile `status.observedGeneration`
/// (the cached object), so the scan runs exactly once per spec change.
pub fn scan_due(
    generation: Option<i64>,
    observed_generation: Option<i64>,
    last_refresh_at: Option<&str>,
    interval: std::time::Duration,
    periodic_enabled: bool,
    now: DateTime<Utc>,
) -> bool {
    if generation != observed_generation {
        return true;
    }
    // The initial scan runs on the generation arm above (first reconcile, spec change);
    // the timed re-scan only fires when periodic refresh is opted in.
    periodic_enabled && refresh_due(last_refresh_at, interval, now)
}

/// `true` when the *no-Job* path may (re-)create the bootstrap Job. The finished
/// Job normally lingers until [`bootstrap_recycle_due`] recycles it — but the
/// kube TTL controller can reap it first (`ttlSecondsAfterFinished`), and an
/// unconditional re-create on that wake would pin the catalog refresh cadence to
/// the Job TTL instead of `catalog.refreshInterval`. So: a repo that is not yet
/// `Ready` always proceeds (first bootstrap / failure retry), and a `Ready` repo
/// proceeds only when the same recycle predicate says a re-run is warranted
/// (refresh due, or the spec changed since the last result was taken).
///
/// Deliberate: a `Failed`/`Degraded` mover-bootstrapped repo keeps re-trying on
/// the Job-TTL cadence (default 1h) — a bounded, infrequent retry against a
/// backend that may have been fixed out-of-band (creds repaired, bucket
/// created). Unlike re-running *succeeded* work this converges, and the
/// in-process filesystem path's stricter `terminal_gate_holds` hard-stop keys
/// on a credential `resourceVersion` the mover path does not pin (yet).
pub fn bootstrap_create_due(
    phase_is_ready: bool,
    generation: Option<i64>,
    observed_generation: Option<i64>,
    last_refresh_at: Option<&str>,
    interval: std::time::Duration,
    periodic_enabled: bool,
    now: DateTime<Utc>,
) -> bool {
    if !phase_is_ready {
        return true;
    }
    bootstrap_recycle_due(
        true,
        generation,
        observed_generation,
        last_refresh_at,
        interval,
        periodic_enabled,
        now,
    )
}

/// A materialized discovered row (one `origin: discovered` `Snapshot` CR of this
/// repository), as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRow {
    /// Namespace the CR lives in.
    pub namespace: String,
    /// CR name.
    pub name: String,
    /// The kopia snapshot id (from the `snapshot-id` label).
    pub snapshot_id: String,
    /// The snapshot's end time (from `status.timing.endTime`), used for
    /// per-identity ordering. Rows written before timing was recorded sort oldest.
    pub end_time: Option<DateTime<Utc>>,
}

/// What a scan decided: entries to materialize and rows to expire.
#[derive(Debug, Default)]
pub struct CatalogPlan<'a> {
    /// Listing entries that need a discovered `Snapshot` CR created.
    pub create: Vec<&'a SnapshotListEntry>,
    /// Existing rows to delete (namespace, name). Deleting a row never touches
    /// the kopia snapshot (discovered rows are forced `Retain`).
    pub expire: Vec<(String, String)>,
}

/// Decide creations and expiries. Pure — see the module docs for the rules.
///
/// `foreign_ignored_ids` (M4, multi-cluster shared repo: [`plan_placements`]'s
/// [`PlacementPass::foreign_ignored_ids`]) is filtered in the exact same spot as
/// `produced_ids`: a `ForeignIgnored`-decided entry is never eligible, so it never
/// consumes a `retain.perIdentity` slot, and — because it still appears in `listing`
/// (only `eligible` excludes it, not `listing`/`listed`) — any PRE-EXISTING discovered
/// row for it expires via the ordinary absence-expiry rule below, on the exact same
/// terms as any other row (safe under truncation once the entry is physically seen).
pub fn plan_catalog<'a>(
    rows: &[CatalogRow],
    produced_ids: &BTreeSet<String>,
    foreign_ignored_ids: &BTreeSet<String>,
    listing: &'a [SnapshotListEntry],
    listing_truncated: bool,
    retain: Option<&CatalogRetain>,
    now: DateTime<Utc>,
) -> CatalogPlan<'a> {
    // Eligible = in the listing, not produced by this repository CR, not decided
    // ForeignIgnored by the placement pass, within the age bound.
    let max_age = retain
        .and_then(|r| r.max_age_days)
        .filter(|d| *d >= 1)
        .map(|d| chrono::Duration::days(d));
    let eligible = listing
        .iter()
        .filter(|e| !produced_ids.contains(&e.id))
        .filter(|e| !foreign_ignored_ids.contains(&e.id))
        .filter(|e| max_age.is_none_or(|a| e.end_time + a > now));

    // Keep-set: the most-recent `perIdentity` eligible entries per identity.
    let per_identity = retain
        .and_then(|r| r.per_identity)
        .filter(|n| *n >= 0)
        .map(|n| n as usize);
    let mut by_identity: BTreeMap<String, Vec<&SnapshotListEntry>> = BTreeMap::new();
    for e in eligible {
        by_identity
            .entry(identity_key(&e.source))
            .or_default()
            .push(e);
    }
    let mut keep: BTreeMap<&str, &SnapshotListEntry> = BTreeMap::new();
    for entries in by_identity.values_mut() {
        entries.sort_by_key(|e| std::cmp::Reverse(e.end_time));
        let cap = per_identity.unwrap_or(entries.len());
        for e in entries.iter().take(cap) {
            keep.insert(e.id.as_str(), e);
        }
    }

    let have: BTreeSet<&str> = rows.iter().map(|r| r.snapshot_id.as_str()).collect();
    let listed: BTreeSet<&str> = listing.iter().map(|e| e.id.as_str()).collect();

    let mut create: Vec<&SnapshotListEntry> = keep
        .values()
        .filter(|e| !have.contains(e.id.as_str()))
        .copied()
        .collect();
    // Newest-first creation order so a creation interrupted mid-batch has
    // materialized the most useful rows first.
    create.sort_by_key(|e| std::cmp::Reverse(e.end_time));

    let expire = rows
        .iter()
        .filter(|r| {
            if keep.contains_key(r.snapshot_id.as_str()) {
                return false;
            }
            // In the listing but outside the keep-set: aged out, over the
            // per-identity cap, or shadowing a produced snapshot — expire (safe
            // even under truncation; we saw the entry). Absent from the listing:
            // deleted repository-side — expire only when the listing is complete.
            listed.contains(r.snapshot_id.as_str()) || !listing_truncated
        })
        .map(|r| (r.namespace.clone(), r.name.clone()))
        .collect();

    CatalogPlan { create, expire }
}

/// Extract this repository's discovered rows from a `Snapshot` LIST (rows carry
/// the `(repository-uid, snapshot-id)` labels). Pure.
pub fn rows_for(repo_uid: &str, snapshots: &[Snapshot]) -> Vec<CatalogRow> {
    snapshots
        .iter()
        .filter_map(|s| {
            let labels = s.labels();
            if labels.get(ORIGIN_LABEL).map(String::as_str) != Some("discovered") {
                return None;
            }
            if labels.get(REPOSITORY_UID_LABEL).map(String::as_str) != Some(repo_uid) {
                return None;
            }
            let id = labels.get(SNAPSHOT_ID_LABEL)?.clone();
            let end_time = s
                .status
                .as_ref()
                .and_then(|st| st.timing.as_ref())
                .and_then(|t| t.end_time.as_deref())
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Utc));
            Some(CatalogRow {
                namespace: s.namespace().unwrap_or_default(),
                name: s.name_any(),
                snapshot_id: id,
                end_time,
            })
        })
        .collect()
}

/// The repository CR a scan runs for, for matching produced `Snapshot` CRs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanOwner<'a> {
    /// A namespaced `Repository`.
    Repository {
        /// CR name.
        name: &'a str,
        /// CR namespace.
        namespace: &'a str,
    },
    /// A cluster-scoped `ClusterRepository`.
    ClusterRepository {
        /// CR name.
        name: &'a str,
    },
}

impl ScanOwner<'_> {
    fn kind(&self) -> RepositoryKind {
        match self {
            ScanOwner::Repository { .. } => RepositoryKind::Repository,
            ScanOwner::ClusterRepository { .. } => RepositoryKind::ClusterRepository,
        }
    }
}

/// The kopia snapshot ids of scheduled/manual `Snapshot` CRs that resolve to
/// this repository CR (via `status.resolved.repository` or the owner reference —
/// the same derivation `Restore` and the kubectl plugin use). These are this
/// cluster's *produced* snapshots: a rescan must never duplicate them as
/// discovered rows. Origin uses [`crate::snapshot::resolve_origin`]'s precedence
/// (status, then label, default manual) — NOT the label alone, because a bare
/// `kubectl create` manual Snapshot may never carry the origin label. Pure.
pub fn produced_ids_for(owner: ScanOwner<'_>, snapshots: &[Snapshot]) -> BTreeSet<String> {
    use kopiur_api::Origin;
    snapshots
        .iter()
        .filter(|s| match crate::snapshot::resolve_origin(s) {
            Origin::Scheduled | Origin::Manual => true,
            Origin::Discovered => false,
        })
        .filter(|s| {
            let Some(rref) = repository_ref_for(s) else {
                return false;
            };
            if rref.kind != owner.kind() {
                return false;
            }
            match owner {
                ScanOwner::Repository { name, namespace } => {
                    let ref_ns = rref
                        .namespace
                        .clone()
                        .or_else(|| s.namespace())
                        .unwrap_or_default();
                    rref.name == name && ref_ns == namespace
                }
                ScanOwner::ClusterRepository { name } => rref.name == name,
            }
        })
        .filter_map(|s| {
            s.status
                .as_ref()
                .and_then(|st| st.snapshot.as_ref())
                .map(|i| i.kopia_snapshot_id.clone())
        })
        .collect()
}

/// Where discovered rows are created.
pub enum Placement<'a> {
    /// A namespaced `Repository`: always its own namespace.
    Namespace(&'a str),
    /// A `ClusterRepository`: the namespace named by the snapshot identity's
    /// hostname when it exists and passes the tenancy gate, else
    /// `catalog.fallbackNamespace`, else the entry is skipped (ADR §2.3).
    Cluster {
        /// The tenancy gate an identity-hostname namespace must pass.
        allowed: &'a AllowedNamespaces,
        /// `catalog.fallbackNamespace` for identities that don't.
        fallback: Option<&'a str>,
    },
}

/// Where the identity-aware placement pass ([`plan_placements`]) landed one
/// hostname, for the materialize loop in [`scan`] to consult (it no longer does
/// its own placement IO).
#[derive(Debug, PartialEq, Eq)]
pub enum PlacementDecision {
    /// Materialize into this namespace.
    Place(String),
    /// Skip + a `DiscoveredSnapshotUnplaced` Warning event — a genuine
    /// misconfiguration (no allowed namespace AND no fallback).
    Unplaced,
    /// Skip + count towards `foreign`; NEVER a Warning — expected and routine on
    /// a repository shared across clusters.
    ForeignIgnored,
}

/// Where a discovered snapshot lands under a `ClusterRepository`'s cross-namespace
/// placement (ADR §2.3), extended for a repository shared across clusters: an entry
/// another cluster wrote is either dropped (`ForeignSnapshots::Ignore`) or collected
/// in `catalog.fallbackNamespace` (`ForeignSnapshots::Fallback`) — see
/// [`classify_hostname`].
///
/// `ns_allowed`: the candidate namespace ([`HostClass::Bare`]/[`HostClass::OwnCluster`]'s
/// `namespace`) exists AND passes `allowedNamespaces`; meaningless for
/// [`HostClass::ForeignCluster`] (no candidate namespace to check) — callers pass
/// `false`.
///
/// `cluster_mode`: whether the consuming repository actually has
/// `identityDefaults.cluster` set. Load-bearing for [`HostClass::Bare`]:
/// `classify_hostname` returns `Bare` BOTH when cluster identity is off (every
/// hostname reads as legacy) AND when it's on but this particular hostname has no
/// `.` — those two situations must not be conflated, or turning cluster identity on
/// would silently change what happens to an already-disallowed bare host. With
/// cluster identity OFF there is no "foreign" concept at all (the validator rejects
/// `foreignSnapshots` in that state, so `foreign` is always the default `Ignore`
/// here too) — a disallowed `Bare` host falls straight to fallback/unplaced exactly
/// as [`crate::cluster_repository`]'s pre-M4 `placement_namespace` always did. With
/// cluster identity ON, an unrecognized bare hostname is treated the same as an
/// explicitly foreign one under `foreign` (a repository with cluster identity turned
/// on cannot tell "a snapshot written before cluster identity existed" from
/// "another party's" once the namespace it names isn't ours): `Ignore` skips it;
/// `Fallback` behaves exactly like the `OwnCluster`/disallowed case below.
///
/// Exhaustive match, no `_ =>`.
pub fn decide_cluster_placement(
    class: HostClass<'_>,
    ns_allowed: bool,
    cluster_mode: bool,
    foreign: ForeignSnapshots,
    fallback: Option<&str>,
) -> PlacementDecision {
    fn fallback_or_unplaced(fallback: Option<&str>) -> PlacementDecision {
        match fallback {
            Some(f) => PlacementDecision::Place(f.to_string()),
            None => PlacementDecision::Unplaced,
        }
    }
    match class {
        HostClass::Bare { namespace } => {
            if ns_allowed {
                return PlacementDecision::Place(namespace.to_string());
            }
            if cluster_mode && matches!(foreign, ForeignSnapshots::Ignore) {
                return PlacementDecision::ForeignIgnored;
            }
            fallback_or_unplaced(fallback)
        }
        HostClass::OwnCluster { namespace } => {
            if ns_allowed {
                PlacementDecision::Place(namespace.to_string())
            } else {
                fallback_or_unplaced(fallback)
            }
        }
        HostClass::ForeignCluster { .. } => match foreign {
            ForeignSnapshots::Ignore => PlacementDecision::ForeignIgnored,
            ForeignSnapshots::Fallback => fallback_or_unplaced(fallback),
        },
    }
}

/// Where a discovered snapshot lands under a namespaced `Repository`: always its
/// own `namespace` for [`HostClass::Bare`]/[`HostClass::OwnCluster`] — a repository's
/// own namespace needs no tenancy gate, unlike a `ClusterRepository`'s cross-namespace
/// placement. [`HostClass::ForeignCluster`] is ALWAYS `ForeignIgnored`, regardless of
/// `catalog.foreignSnapshots`: a namespaced `Repository` has no `fallbackNamespace`
/// concept, so even a `Fallback` policy value somehow reaching here (the validator
/// rejects that combination on a non-cluster-scoped repository; this is a defensive
/// backstop) must never fall through to materializing a foreign entry into the repo's
/// own namespace by accident.
///
/// Exhaustive match, no `_ =>`.
pub fn decide_namespace_placement(class: HostClass<'_>, namespace: &str) -> PlacementDecision {
    match class {
        HostClass::Bare { .. } | HostClass::OwnCluster { .. } => {
            PlacementDecision::Place(namespace.to_string())
        }
        HostClass::ForeignCluster { .. } => PlacementDecision::ForeignIgnored,
    }
}

/// Every distinct candidate namespace name ([`HostClass::Bare`]/[`HostClass::OwnCluster`]'s
/// `namespace`) that at least one entry in `listing` needs a tenancy-gate answer for,
/// under `cluster`. Thin helper so [`scan`] knows exactly which (cached) `Namespace`
/// GETs to perform; [`plan_placements`] then takes the resolved answers as a plain
/// map, so it stays pure and unit-testable with a stub. Pure.
pub fn candidate_namespaces<'a>(
    listing: &'a [SnapshotListEntry],
    cluster: Option<&str>,
) -> BTreeSet<&'a str> {
    listing
        .iter()
        .filter_map(|e| match classify_hostname(&e.source.host, cluster) {
            HostClass::Bare { namespace } | HostClass::OwnCluster { namespace } => Some(namespace),
            HostClass::ForeignCluster { .. } => None,
        })
        .collect()
}

/// Outcome of the identity-aware placement pass ([`plan_placements`]) over a FULL
/// kopia listing — see the module docs for the pipeline this feeds into.
#[derive(Debug, Default)]
pub struct PlacementPass {
    /// Per-hostname decision the materialize loop in [`scan`] consults.
    pub decisions: BTreeMap<String, PlacementDecision>,
    /// Kopia ids of listing entries whose host decided [`PlacementDecision::ForeignIgnored`] —
    /// fed to [`plan_catalog`] exactly like `produced_ids`.
    pub foreign_ignored_ids: BTreeSet<String>,
    /// Entries this scan counted as foreign: every foreign-SUFFIXED entry, plus
    /// bare-disallowed hosts only when they decided [`PlacementDecision::ForeignIgnored`]
    /// (under `Fallback`, a bare-disallowed host materializes into the fallback and is
    /// NOT counted — see the `ForeignSnapshots` doc in `common/cache.rs`, which states
    /// the same rule). Feeds `status.catalog.foreignSnapshotCount` /
    /// `kopiur_repo_foreign_snapshots` (plus any count the mover's prefilter already
    /// dropped before this pass ever saw them — the caller adds that in, never
    /// double-counted).
    pub foreign_count: i64,
    /// Bounded top-N (by count desc) foreign-suffix breakdown for the scan's info
    /// log — NEVER surfaced in status (cardinality is foreign-writer-controlled).
    /// Keyed by the [`HostClass::ForeignCluster`] suffix, or the full hostname for a
    /// bare host treated as foreign (it carries no suffix).
    pub foreign_suffix_counts: BTreeMap<String, i64>,
}

/// Run the identity-aware placement pass over a FULL kopia `listing`, BEFORE
/// [`plan_catalog`] (see the module docs). Pure: `ns_allowed` is the tenancy-gate
/// answer for every [`candidate_namespaces`] entry, ALREADY resolved by the caller
/// (a cached `Namespace` GET + [`validate::validate_consumer_against_cluster_repo`]
/// for [`scan`]; a stub in tests) — no IO happens here.
pub fn plan_placements(
    listing: &[SnapshotListEntry],
    cluster: Option<&str>,
    foreign: ForeignSnapshots,
    placement: &Placement<'_>,
    ns_allowed: &BTreeMap<String, bool>,
) -> PlacementPass {
    let cluster_mode = cluster.is_some_and(|c| !c.is_empty());

    // Pass 1: one decision per DISTINCT hostname (a host's classification/decision
    // never varies across its entries within one scan).
    let mut decisions: BTreeMap<String, PlacementDecision> = BTreeMap::new();
    let mut foreign_suffix_by_host: BTreeMap<String, Option<String>> = BTreeMap::new();
    for entry in listing {
        let host = entry.source.host.as_str();
        if decisions.contains_key(host) {
            continue;
        }
        let class = classify_hostname(host, cluster);
        foreign_suffix_by_host.insert(
            host.to_string(),
            match class {
                HostClass::ForeignCluster { suffix } => Some(suffix.to_string()),
                HostClass::Bare { .. } | HostClass::OwnCluster { .. } => None,
            },
        );
        let decision = match placement {
            Placement::Namespace(ns) => decide_namespace_placement(class, ns),
            Placement::Cluster { fallback, .. } => {
                // The tenancy gate itself (`allowedNamespaces`) was already applied by
                // the caller when it resolved `ns_allowed`; this pure pass just reads
                // the answer for the candidate namespace `class` names.
                let candidate_allowed = match class {
                    HostClass::Bare { namespace } | HostClass::OwnCluster { namespace } => {
                        ns_allowed.get(namespace).copied().unwrap_or(false)
                    }
                    HostClass::ForeignCluster { .. } => false,
                };
                decide_cluster_placement(class, candidate_allowed, cluster_mode, foreign, *fallback)
            }
        };
        decisions.insert(host.to_string(), decision);
    }

    // Pass 2: per-ENTRY accounting (a host with N entries contributes N to
    // `foreign_count`/the suffix breakdown — `status.catalog.foreignSnapshotCount`
    // counts snapshots, not hosts).
    let mut foreign_ignored_ids: BTreeSet<String> = BTreeSet::new();
    let mut foreign_count: i64 = 0;
    let mut foreign_suffix_counts: BTreeMap<String, i64> = BTreeMap::new();
    for entry in listing {
        let host = entry.source.host.as_str();
        let Some(decision) = decisions.get(host) else {
            // Invariant: pass 1 above classified every distinct host in `listing`.
            // Never observed; defensive rather than a panic in a reconcile path.
            tracing::error!(
                host,
                "no placement decision cached for a listing host (bug)"
            );
            continue;
        };
        let foreign_suffix = foreign_suffix_by_host.get(host).cloned().flatten();
        let is_foreign = foreign_suffix.is_some() || *decision == PlacementDecision::ForeignIgnored;
        if is_foreign {
            foreign_count += 1;
            let key = foreign_suffix.unwrap_or_else(|| host.to_string());
            *foreign_suffix_counts.entry(key).or_insert(0) += 1;
        }
        if *decision == PlacementDecision::ForeignIgnored {
            foreign_ignored_ids.insert(entry.id.clone());
        }
    }

    PlacementPass {
        decisions,
        foreign_ignored_ids,
        foreign_count,
        foreign_suffix_counts,
    }
}

/// What a [`scan`] did, for the caller's status patch / metrics / events.
#[derive(Debug, Default)]
pub struct ScanOutcome {
    /// Discovered rows created this scan.
    pub created: i64,
    /// Discovered rows expired (CR deleted; kopia snapshot untouched).
    pub expired: i64,
    /// Discovered rows of this repository after the scan (the
    /// `status.catalog.discoveredBackupCount` value).
    pub discovered: i64,
    /// `ClusterRepository` only: identity hostnames that mapped to no allowed
    /// namespace and had no `fallbackNamespace` — their snapshots got no row.
    /// The caller surfaces these (Event + log) with the fix.
    pub unplaced_hosts: BTreeSet<String>,
    /// Snapshots in this scan's listing classified as another cluster's (or a bare
    /// host treated the same way — see [`PlacementPass::foreign_count`]). The
    /// caller adds any count the mover's prefilter already dropped before this
    /// scan ever saw them (never double-counted: a mover-dropped entry never
    /// reaches this scan). The `status.catalog.foreignSnapshotCount` /
    /// `kopiur_repo_foreign_snapshots` value.
    pub foreign: i64,
}

/// Run a catalog scan: LIST the relevant `Snapshot` CRs, run the identity-aware
/// placement pass ([`plan_placements`]), [`plan_catalog`], then create/expire rows.
/// The caller supplies the kopia listing (in-process for bare-path filesystem, from
/// the bootstrap Job's result for everything else) and patches its own status from
/// the returned outcome.
///
/// `cluster` is the consuming repository's `identityDefaults.cluster` (`Repository`
/// or `ClusterRepository`; `None` when the repository has no cluster identity set).
#[allow(clippy::too_many_arguments)]
pub async fn scan(
    ctx: &Context,
    owner: ScanOwner<'_>,
    owner_ref: OwnerReference,
    repo_uid: &str,
    placement: Placement<'_>,
    cluster: Option<&str>,
    catalog: Option<&CatalogBounds>,
    listing: &[SnapshotListEntry],
    listing_truncated: bool,
) -> Result<ScanOutcome> {
    let repo_name = match owner {
        ScanOwner::Repository { name, .. } | ScanOwner::ClusterRepository { name } => name,
    };

    // One install-scope-wide LIST serves both sides of the plan: this repository's
    // discovered rows (by the dedup labels — always controller-stamped) AND the
    // produced snapshots whose ids must never be re-discovered. Cluster-wide in
    // cluster scope on purpose: a ClusterRepository's rows live in many
    // namespaces, and a SnapshotPolicy may reference a Repository across
    // namespaces. In namespaced scope the LIST is narrowed to the release
    // namespace — Role RBAC makes a cluster-wide LIST a permanent 403 (which
    // wedged every namespaced Repository at Initializing), and both cross-
    // namespace cases above are structurally out of a namespaced install's
    // world. No label selector: a bare `kubectl create` manual Snapshot may
    // carry no origin label, and missing it here would duplicate it as a
    // discovered row. The LIST is refresh-gated (default 1h), not per-reconcile.
    let all_api: Api<Snapshot> = crate::controllers::scoped_api(&ctx.client, &ctx.watch_scope);
    let all_snapshots = all_api.list(&ListParams::default()).await?.items;
    let rows = rows_for(repo_uid, &all_snapshots);
    let produced_ids = produced_ids_for(owner, &all_snapshots);

    // Identity-aware placement pass over the FULL listing, BEFORE plan_catalog (see
    // the module docs): thin IO here (one cached `Namespace` GET per distinct
    // candidate namespace, only for the `Cluster` placement kind), then the pure
    // decision in `plan_placements`.
    let foreign = CatalogBounds::effective_foreign_snapshots(catalog);
    let mut ns_allowed: BTreeMap<String, bool> = BTreeMap::new();
    if let Placement::Cluster { allowed, .. } = &placement {
        for candidate in candidate_namespaces(listing, cluster) {
            let ns_api: Api<Namespace> = Api::all(ctx.client.clone());
            let labels = ns_api
                .get_opt(candidate)
                .await?
                .map(|n| n.metadata.labels.unwrap_or_default());
            let ok = labels.as_ref().is_some_and(|l| {
                validate::validate_consumer_against_cluster_repo(
                    candidate,
                    repo_name,
                    allowed,
                    Some(l),
                )
                .is_ok()
            });
            ns_allowed.insert(candidate.to_string(), ok);
        }
    }
    let pass = plan_placements(listing, cluster, foreign, &placement, &ns_allowed);

    let retain = catalog.and_then(|c| c.retain.as_ref());
    let plan = plan_catalog(
        &rows,
        &produced_ids,
        &pass.foreign_ignored_ids,
        listing,
        listing_truncated,
        retain,
        Utc::now(),
    );

    let mut outcome = ScanOutcome {
        foreign: pass.foreign_count,
        ..Default::default()
    };

    for entry in &plan.create {
        let host = entry.source.host.as_str();
        let target_ns = match pass.decisions.get(host) {
            Some(PlacementDecision::Place(ns)) => ns.clone(),
            Some(PlacementDecision::Unplaced) | None => {
                outcome.unplaced_hosts.insert(host.to_string());
                continue;
            }
            // Excluded from `plan.create` via `foreign_ignored_ids` above; this arm
            // is defensive (never actually reached).
            Some(PlacementDecision::ForeignIgnored) => continue,
        };
        materialize_discovered(ctx, &owner_ref, &target_ns, repo_name, repo_uid, entry).await?;
        outcome.created += 1;
    }

    for (ns, name) in &plan.expire {
        let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), ns);
        match api.delete(name, &DeleteParams::default()).await {
            Ok(_) => outcome.expired += 1,
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}
            Err(e) => return Err(Error::Kube(e)),
        }
    }

    outcome.discovered = (rows.len() as i64 - outcome.expired).max(0) + outcome.created;
    if outcome.created > 0 || outcome.expired > 0 || outcome.foreign > 0 {
        // Bounded top-N (never in status: cardinality is foreign-writer-controlled).
        let mut top: Vec<(&String, &i64)> = pass.foreign_suffix_counts.iter().collect();
        top.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        top.truncate(5);
        tracing::info!(
            repo = repo_name,
            created = outcome.created,
            expired = outcome.expired,
            discovered = outcome.discovered,
            foreign = outcome.foreign,
            foreign_top_suffixes = ?top,
            "catalog scan reconciled discovered Snapshot CRs"
        );
    }
    Ok(outcome)
}

/// Create one `origin: discovered` `Snapshot` CR for a listing entry.
/// `deletionPolicy` is FORCED to `Retain` (the operator never deletes a
/// discovered snapshot, §4.5); identity, timing, and size come from the kopia
/// listing so `kubectl kopiur snapshots list` shows real data for foreign rows.
async fn materialize_discovered(
    ctx: &Context,
    owner: &OwnerReference,
    namespace: &str,
    repo_name: &str,
    repo_uid: &str,
    entry: &SnapshotListEntry,
) -> Result<()> {
    use kopiur_api::common::{DeletionPolicy, ResolvedIdentity};
    use kopiur_api::snapshot::{
        SnapshotInfo, SnapshotSpec, SnapshotStats, SnapshotStatus, SnapshotTiming,
    };
    use kopiur_api::{Origin, SnapshotPhase};

    // CR name: stable from the (short) snapshot id, namespaced under the repo.
    let short = entry.id.chars().take(16).collect::<String>();
    let cr_name = format!("{repo_name}-disc-{short}");

    let mut labels = BTreeMap::new();
    labels.insert(ORIGIN_LABEL.to_string(), "discovered".to_string());
    labels.insert(REPOSITORY_UID_LABEL.to_string(), repo_uid.to_string());
    labels.insert(SNAPSHOT_ID_LABEL.to_string(), entry.id.clone());

    let mut backup = Snapshot::new(
        &cr_name,
        SnapshotSpec {
            policy_ref: None,
            tags: None,
            failure_policy: None,
            // Forced Retain for discovered (webhook would reject otherwise).
            deletion_policy: Some(DeletionPolicy::Retain),
            // Discovered snapshots are not pinned by the operator.
            pin: false,
            // Discovered snapshots never carry a templated description (out
            // of scope for M4 — description is per-invocation only).
            description: None,
        },
    );
    backup.metadata = io::child_meta(&cr_name, namespace, labels, Some(owner.clone()));
    backup.status = Some(SnapshotStatus {
        phase: Some(SnapshotPhase::Discovered),
        origin: Some(Origin::Discovered),
        snapshot: Some(SnapshotInfo {
            kopia_snapshot_id: entry.id.clone(),
            identity: ResolvedIdentity {
                username: entry.source.user_name.clone(),
                hostname: entry.source.host.clone(),
                source_path: Some(entry.source.path.clone()),
            },
        }),
        timing: Some(SnapshotTiming {
            start_time: Some(entry.start_time.to_rfc3339()),
            end_time: Some(entry.end_time.to_rfc3339()),
            duration_seconds: Some((entry.end_time - entry.start_time).num_seconds()),
        }),
        stats: Some(SnapshotStats {
            size_bytes: i64::try_from(entry.stats.total_size).ok(),
            ..Default::default()
        }),
        ..Default::default()
    });

    let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), namespace);
    // Create the CR; the discovered status is then PATCHed onto the subresource.
    match io::apply(&api, &cr_name, &backup).await {
        Ok(_) => {}
        Err(Error::Kube(kube::Error::Api(ae))) if ae.code == 409 => return Ok(()),
        Err(e) => return Err(e),
    }
    io::patch_status(
        &api,
        &cr_name,
        serde_json::to_value(backup.status.unwrap_or_default())?,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_kopia::SnapshotStats;

    fn entry(id: &str, identity: (&str, &str, &str), end: DateTime<Utc>) -> SnapshotListEntry {
        SnapshotListEntry {
            id: id.into(),
            source: SnapshotSource {
                user_name: identity.0.into(),
                host: identity.1.into(),
                path: identity.2.into(),
            },
            description: String::new(),
            start_time: end - chrono::Duration::seconds(60),
            end_time: end,
            stats: SnapshotStats::default(),
            root_entry: None,
            retention_reason: vec![],
        }
    }

    fn row(name: &str, id: &str, end: Option<DateTime<Utc>>) -> CatalogRow {
        CatalogRow {
            namespace: "ns".into(),
            name: name.into(),
            snapshot_id: id.into(),
            end_time: end,
        }
    }

    fn t(mins_ago: i64) -> DateTime<Utc> {
        Utc::now() - chrono::Duration::minutes(mins_ago)
    }

    fn ids<'a>(plan: &'a CatalogPlan<'_>) -> Vec<&'a str> {
        plan.create.iter().map(|e| e.id.as_str()).collect()
    }

    fn expired<'a>(plan: &'a CatalogPlan<'_>) -> Vec<&'a str> {
        plan.expire.iter().map(|(_, n)| n.as_str()).collect()
    }

    #[test]
    fn materializes_unseen_entries_and_dedups_existing() {
        let listing = vec![
            entry("aaa", ("u", "h", "/p"), t(10)),
            entry("bbb", ("u", "h", "/p"), t(5)),
        ];
        let rows = vec![row("r-aaa", "aaa", Some(t(10)))];
        let plan = plan_catalog(
            &rows,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &listing,
            false,
            None,
            Utc::now(),
        );
        assert_eq!(ids(&plan), vec!["bbb"]);
        assert!(plan.expire.is_empty());
    }

    #[test]
    fn produced_snapshots_never_become_discovered_rows() {
        // A rescan of a repository this cluster writes to must not duplicate its
        // own scheduled/manual backups as discovered rows.
        let listing = vec![
            entry("ours", ("app", "ns", "/data"), t(5)),
            entry("foreign", ("legacy", "old-host", "/data"), t(7)),
        ];
        let produced: BTreeSet<String> = ["ours".to_string()].into();
        let plan = plan_catalog(
            &[],
            &produced,
            &BTreeSet::new(),
            &listing,
            false,
            None,
            Utc::now(),
        );
        assert_eq!(ids(&plan), vec!["foreign"]);
    }

    #[test]
    fn a_stale_discovered_row_shadowing_a_produced_snapshot_is_expired() {
        // Cleanup path for rows created by the old (pre-dedup) scan.
        let listing = vec![entry("ours", ("app", "ns", "/data"), t(5))];
        let produced: BTreeSet<String> = ["ours".to_string()].into();
        let rows = vec![row("r-ours", "ours", Some(t(5)))];
        let plan = plan_catalog(
            &rows,
            &produced,
            &BTreeSet::new(),
            &listing,
            false,
            None,
            Utc::now(),
        );
        assert!(plan.create.is_empty());
        assert_eq!(expired(&plan), vec!["r-ours"]);
    }

    #[test]
    fn per_identity_cap_is_per_identity_not_global() {
        // Identity A has 3 snapshots, identity B has 1; perIdentity=2 must keep
        // the 2 newest of A AND B's single one (a global cap would starve B).
        let listing = vec![
            entry("a1", ("u", "a", "/p"), t(30)),
            entry("a2", ("u", "a", "/p"), t(20)),
            entry("a3", ("u", "a", "/p"), t(10)),
            entry("b1", ("u", "b", "/p"), t(40)),
        ];
        let retain = CatalogRetain {
            per_identity: Some(2),
            max_age_days: None,
        };
        let plan = plan_catalog(
            &[],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &listing,
            false,
            Some(&retain),
            Utc::now(),
        );
        let mut got = ids(&plan);
        got.sort();
        assert_eq!(got, vec!["a2", "a3", "b1"]);
    }

    #[test]
    fn per_identity_zero_disables_materialization() {
        let listing = vec![entry("aaa", ("u", "h", "/p"), t(5))];
        let retain = CatalogRetain {
            per_identity: Some(0),
            max_age_days: None,
        };
        let plan = plan_catalog(
            &[],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &listing,
            false,
            Some(&retain),
            Utc::now(),
        );
        assert!(plan.create.is_empty());
    }

    #[test]
    fn over_cap_rows_are_expired_oldest_first_semantics() {
        // 3 rows exist for one identity; perIdentity=1 keeps only the newest and
        // expires the other two CRs (never the kopia snapshots).
        let listing = vec![
            entry("a1", ("u", "a", "/p"), t(30)),
            entry("a2", ("u", "a", "/p"), t(20)),
            entry("a3", ("u", "a", "/p"), t(10)),
        ];
        let rows = vec![
            row("r-a1", "a1", Some(t(30))),
            row("r-a2", "a2", Some(t(20))),
            row("r-a3", "a3", Some(t(10))),
        ];
        let retain = CatalogRetain {
            per_identity: Some(1),
            max_age_days: None,
        };
        let plan = plan_catalog(
            &rows,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &listing,
            false,
            Some(&retain),
            Utc::now(),
        );
        assert!(plan.create.is_empty());
        let mut gone = expired(&plan);
        gone.sort();
        assert_eq!(gone, vec!["r-a1", "r-a2"]);
    }

    #[test]
    fn max_age_days_excludes_old_snapshots_and_expires_their_rows() {
        let old = Utc::now() - chrono::Duration::days(120);
        let listing = vec![
            entry("old", ("u", "h", "/p"), old),
            entry("new", ("u", "h", "/p"), t(5)),
        ];
        let rows = vec![row("r-old", "old", Some(old))];
        let retain = CatalogRetain {
            per_identity: None,
            max_age_days: Some(90),
        };
        let plan = plan_catalog(
            &rows,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &listing,
            false,
            Some(&retain),
            Utc::now(),
        );
        assert_eq!(ids(&plan), vec!["new"]);
        assert_eq!(expired(&plan), vec!["r-old"]);
    }

    #[test]
    fn absent_rows_expire_only_when_the_listing_is_complete() {
        // The row's snapshot was deleted repository-side.
        let rows = vec![row("r-gone", "gone", Some(t(10)))];
        let listing = vec![entry("still", ("u", "h", "/p"), t(5))];
        // Complete listing → stale row expires.
        let plan = plan_catalog(
            &rows,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &listing,
            false,
            None,
            Utc::now(),
        );
        assert_eq!(expired(&plan), vec!["r-gone"]);
        // Truncated listing → absence is unknowable; the row survives.
        let plan = plan_catalog(
            &rows,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &listing,
            true,
            None,
            Utc::now(),
        );
        assert!(plan.expire.is_empty());
    }

    #[test]
    fn truncated_listing_still_expires_rows_it_can_see_are_over_cap() {
        let listing = vec![
            entry("a1", ("u", "a", "/p"), t(30)),
            entry("a2", ("u", "a", "/p"), t(10)),
        ];
        let rows = vec![
            row("r-a1", "a1", Some(t(30))),
            row("r-a2", "a2", Some(t(10))),
        ];
        let retain = CatalogRetain {
            per_identity: Some(1),
            max_age_days: None,
        };
        let plan = plan_catalog(
            &rows,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &listing,
            true,
            Some(&retain),
            Utc::now(),
        );
        assert_eq!(expired(&plan), vec!["r-a1"]);
    }

    #[test]
    fn refresh_due_gates_on_the_stamp() {
        let now = Utc::now();
        let interval = std::time::Duration::from_secs(3600);
        // Never scanned → due.
        assert!(refresh_due(None, interval, now));
        // Unparseable stamp → due (defensive).
        assert!(refresh_due(Some("not-a-time"), interval, now));
        // Fresh → not due.
        let fresh = (now - chrono::Duration::minutes(5)).to_rfc3339();
        assert!(!refresh_due(Some(&fresh), interval, now));
        // Stale → due.
        let stale = (now - chrono::Duration::minutes(61)).to_rfc3339();
        assert!(refresh_due(Some(&stale), interval, now));
    }

    #[test]
    fn reverify_due_honors_once_and_only_while_ready() {
        // No request → never.
        assert!(!reverify_due(None, None, true));
        // Fresh request, Ready, never honored → force the re-probe.
        assert!(reverify_due(Some("t1"), None, true));
        // Already honored this exact token → no-op (the loop guard).
        assert!(!reverify_due(Some("t1"), Some("t1"), true));
        // A genuinely newer token re-fires.
        assert!(reverify_due(Some("t2"), Some("t1"), true));
        // Not Ready (already re-probing / Failed) → never force; the gate holds.
        assert!(!reverify_due(Some("t2"), Some("t1"), false));
    }

    #[test]
    fn should_request_reverify_rate_limits_on_the_existing_stamp() {
        let now = Utc::now();
        let min = std::time::Duration::from_secs(60);
        // No prior request → request.
        assert!(should_request_reverify(None, now, min));
        // Unparseable → request (defensive).
        assert!(should_request_reverify(Some("nope"), now, min));
        // Within the window → skip (a wave of failures collapses to one re-probe).
        let recent = (now - chrono::Duration::seconds(30)).to_rfc3339();
        assert!(!should_request_reverify(Some(&recent), now, min));
        // Past the window → request again.
        let old = (now - chrono::Duration::seconds(61)).to_rfc3339();
        assert!(should_request_reverify(Some(&old), now, min));
    }

    #[test]
    fn bootstrap_recycle_requires_ready_and_fires_on_due_or_spec_change() {
        let now = Utc::now();
        let interval = std::time::Duration::from_secs(3600);
        let fresh = (now - chrono::Duration::minutes(5)).to_rfc3339();
        let stale = (now - chrono::Duration::minutes(61)).to_rfc3339();
        // Not Ready: never recycle (a Failed bootstrap is gated elsewhere).
        assert!(!bootstrap_recycle_due(
            false,
            Some(2),
            Some(1),
            None,
            interval,
            true,
            now
        ));
        // Ready + spec changed → recycle even when fresh — independent of the
        // periodic-refresh flag (a re-pointed backend must always re-bootstrap).
        assert!(bootstrap_recycle_due(
            true,
            Some(2),
            Some(1),
            Some(&fresh),
            interval,
            false,
            now
        ));
        // Ready + same generation + fresh → keep the finished Job.
        assert!(!bootstrap_recycle_due(
            true,
            Some(2),
            Some(2),
            Some(&fresh),
            interval,
            true,
            now
        ));
        // Ready + same generation + stale + periodic ON → recycle for a fresh listing.
        assert!(bootstrap_recycle_due(
            true,
            Some(2),
            Some(2),
            Some(&stale),
            interval,
            true,
            now
        ));
        // Ready + same generation + stale + periodic OFF (default) → do NOT recycle
        // on the timer: one-time bootstrap semantics.
        assert!(!bootstrap_recycle_due(
            true,
            Some(2),
            Some(2),
            Some(&stale),
            interval,
            false,
            now
        ));
    }

    // Regression guard (caught by the catalog_retain e2e): a spec change
    // recycles the bootstrap Job for a fresh listing, but the SCAN of that
    // result was gated on the timed refresh alone — a tightened
    // `catalog.retain` only expired rows at the next refreshInterval, not on
    // the edit that asked for it.
    #[test]
    fn scan_due_fires_on_spec_change_even_when_the_timed_refresh_is_not() {
        let now = Utc::now();
        let interval = std::time::Duration::from_secs(3600);
        let fresh = (now - chrono::Duration::minutes(5)).to_rfc3339();
        // Spec changed (gen != observed) + fresh stamp → scan NOW, regardless of the
        // periodic-refresh flag (the initial/spec-change scan is always honored).
        assert!(scan_due(
            Some(3),
            Some(2),
            Some(&fresh),
            interval,
            false,
            now
        ));
        // Settled generation + fresh stamp → byte-stable, no scan (the
        // status-churn rule).
        assert!(!scan_due(
            Some(3),
            Some(3),
            Some(&fresh),
            interval,
            true,
            now
        ));
        // Settled generation + stale stamp + periodic ON → the timed refresh fires.
        let stale = (now - chrono::Duration::minutes(61)).to_rfc3339();
        assert!(scan_due(
            Some(3),
            Some(3),
            Some(&stale),
            interval,
            true,
            now
        ));
        // Settled generation + stale stamp + periodic OFF (default) → no timed re-scan.
        assert!(!scan_due(
            Some(3),
            Some(3),
            Some(&stale),
            interval,
            false,
            now
        ));
        // Never scanned + periodic OFF → still no timed scan (the initial scan runs on
        // the generation arm, not this timer).
        assert!(!scan_due(Some(1), Some(1), None, interval, false, now));
    }

    // Regression guard for the TTL-reap loop: when the kube TTL controller
    // deletes the finished bootstrap Job before the refresh interval elapses,
    // the no-Job path must NOT re-create it — otherwise the Job TTL (default
    // 1h) silently overrides `catalog.refreshInterval`.
    #[test]
    fn bootstrap_create_after_ttl_reap_waits_for_the_refresh_interval() {
        let now = Utc::now();
        let interval = std::time::Duration::from_secs(3600);
        let fresh = (now - chrono::Duration::minutes(5)).to_rfc3339();
        let stale = (now - chrono::Duration::minutes(61)).to_rfc3339();
        // Not Ready → always proceed (first bootstrap / failure retry), regardless
        // of the periodic flag.
        assert!(bootstrap_create_due(
            false,
            Some(1),
            None,
            None,
            interval,
            false,
            now
        ));
        // Ready + same generation + fresh scan → HOLD: the reaped Job must not
        // come back until the refresh is due.
        assert!(!bootstrap_create_due(
            true,
            Some(2),
            Some(2),
            Some(&fresh),
            interval,
            true,
            now
        ));
        // Ready + refresh due + periodic ON → re-create for a fresh listing.
        assert!(bootstrap_create_due(
            true,
            Some(2),
            Some(2),
            Some(&stale),
            interval,
            true,
            now
        ));
        // Ready + refresh due + periodic OFF (default) → HOLD: no timed re-create.
        assert!(!bootstrap_create_due(
            true,
            Some(2),
            Some(2),
            Some(&stale),
            interval,
            false,
            now
        ));
        // Ready + spec changed → re-create even when fresh (independent of the flag).
        assert!(bootstrap_create_due(
            true,
            Some(3),
            Some(2),
            Some(&fresh),
            interval,
            false,
            now
        ));
        // Ready but never stamped + periodic ON → defensive re-run.
        assert!(bootstrap_create_due(
            true,
            Some(2),
            Some(2),
            None,
            interval,
            true,
            now
        ));
    }

    #[test]
    fn rows_and_produced_extraction_respect_labels_and_refs() {
        use kopiur_api::common::{RepositoryKind, RepositoryRef};
        use kopiur_api::snapshot::{ResolvedSnapshot, SnapshotInfo, SnapshotStatus};

        fn snap(
            name: &str,
            ns: &str,
            origin: &str,
            extra_labels: &[(&str, &str)],
            status: Option<SnapshotStatus>,
        ) -> Snapshot {
            let mut s = Snapshot::new(
                name,
                kopiur_api::snapshot::SnapshotSpec {
                    policy_ref: None,
                    tags: None,
                    failure_policy: None,
                    deletion_policy: None,
                    pin: false,
                    description: None,
                },
            );
            let mut labels = BTreeMap::new();
            labels.insert(ORIGIN_LABEL.to_string(), origin.to_string());
            for (k, v) in extra_labels {
                labels.insert((*k).to_string(), (*v).to_string());
            }
            s.metadata.namespace = Some(ns.to_string());
            s.metadata.labels = Some(labels);
            s.status = status;
            s
        }

        let discovered = snap(
            "repo-disc-aaa",
            "ns1",
            "discovered",
            &[(REPOSITORY_UID_LABEL, "uid-1"), (SNAPSHOT_ID_LABEL, "aaa")],
            None,
        );
        let other_uid = snap(
            "other-disc-bbb",
            "ns1",
            "discovered",
            &[(REPOSITORY_UID_LABEL, "uid-2"), (SNAPSHOT_ID_LABEL, "bbb")],
            None,
        );
        let produced = snap(
            "nightly-1",
            "ns1",
            "scheduled",
            &[],
            Some(SnapshotStatus {
                snapshot: Some(SnapshotInfo {
                    kopia_snapshot_id: "ccc".into(),
                    identity: kopiur_api::common::ResolvedIdentity {
                        username: "u".into(),
                        hostname: "h".into(),
                        source_path: Some("/p".into()),
                    },
                }),
                resolved: Some(ResolvedSnapshot {
                    repository: Some(RepositoryRef {
                        kind: RepositoryKind::Repository,
                        name: "repo".into(),
                        namespace: None,
                    }),
                    sources: vec![],
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );

        let all = vec![discovered, other_uid, produced];
        let rows = rows_for("uid-1", &all);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].snapshot_id, "aaa");

        // A bare `kubectl create` manual Snapshot: NO origin label at all, no
        // status.origin — resolve_origin defaults it to Manual, so its id must
        // still be excluded from discovery (label-selector matching would miss it).
        let mut bare = snap(
            "manual-1",
            "ns1",
            "ignored",
            &[],
            Some(SnapshotStatus {
                snapshot: Some(SnapshotInfo {
                    kopia_snapshot_id: "ddd".into(),
                    identity: kopiur_api::common::ResolvedIdentity {
                        username: "u".into(),
                        hostname: "h".into(),
                        source_path: Some("/p".into()),
                    },
                }),
                resolved: Some(ResolvedSnapshot {
                    repository: Some(RepositoryRef {
                        kind: RepositoryKind::Repository,
                        name: "repo".into(),
                        namespace: None,
                    }),
                    sources: vec![],
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );
        bare.metadata.labels = None;

        let all = {
            let mut v = all;
            v.push(bare);
            v
        };
        // The produced snapshots resolve to Repository/repo in their own namespace.
        let ids = produced_ids_for(
            ScanOwner::Repository {
                name: "repo",
                namespace: "ns1",
            },
            &all,
        );
        assert_eq!(ids, ["ccc".to_string(), "ddd".to_string()].into());
        // …but not to a different Repository, nor to a ClusterRepository.
        assert!(
            produced_ids_for(
                ScanOwner::Repository {
                    name: "repo",
                    namespace: "other-ns",
                },
                &all,
            )
            .is_empty()
        );
        assert!(produced_ids_for(ScanOwner::ClusterRepository { name: "repo" }, &all).is_empty());
    }

    #[test]
    fn reconcile_interval_honors_fast_refresh_but_caps_at_five_minutes() {
        // No catalog / slow refresh → the usual 5 minutes.
        assert_eq!(
            reconcile_interval(None),
            std::time::Duration::from_secs(300)
        );
        let slow: CatalogBounds = serde_json::from_value(
            serde_json::json!({ "periodicRefresh": true, "refreshInterval": "2h" }),
        )
        .unwrap();
        assert_eq!(
            reconcile_interval(Some(&slow)),
            std::time::Duration::from_secs(300)
        );
        // A faster refresh (with periodic ON) shortens the requeue so the cadence fires.
        let fast: CatalogBounds = serde_json::from_value(
            serde_json::json!({ "periodicRefresh": true, "refreshInterval": "30s" }),
        )
        .unwrap();
        assert_eq!(
            reconcile_interval(Some(&fast)),
            std::time::Duration::from_secs(30)
        );
        // Periodic refresh OFF (default): the interval is inert — stay at 5 minutes
        // even with a fast `refreshInterval` set.
        let fast_off: CatalogBounds =
            serde_json::from_value(serde_json::json!({ "refreshInterval": "30s" })).unwrap();
        assert_eq!(
            reconcile_interval(Some(&fast_off)),
            std::time::Duration::from_secs(300)
        );
    }

    #[test]
    fn identity_key_is_user_at_host_colon_path() {
        let src = SnapshotSource {
            user_name: "legacy".into(),
            host: "media".into(),
            path: "/data".into(),
        };
        assert_eq!(identity_key(&src), "legacy@media:/data");
    }

    // --- decide_cluster_placement / decide_namespace_placement: decision table ---
    // Row numbers refer to the table in the M4 design doc / catalog.rs module doc.

    #[test]
    fn row1_bare_ns_allowed_places_into_the_candidate_namespace() {
        let class = HostClass::Bare {
            namespace: "billing",
        };
        for cluster_mode in [false, true] {
            for foreign in [ForeignSnapshots::Ignore, ForeignSnapshots::Fallback] {
                assert_eq!(
                    decide_cluster_placement(
                        class,
                        true,
                        cluster_mode,
                        foreign,
                        Some("fallback-ns")
                    ),
                    PlacementDecision::Place("billing".to_string())
                );
            }
        }
    }

    #[test]
    fn row2_bare_disallowed_fallback_places_regardless_of_cluster_mode() {
        let class = HostClass::Bare { namespace: "evil" };
        assert_eq!(
            decide_cluster_placement(
                class,
                false,
                true,
                ForeignSnapshots::Fallback,
                Some("kopia-system")
            ),
            PlacementDecision::Place("kopia-system".to_string())
        );
    }

    #[test]
    fn row3_bare_disallowed_fallback_no_fallback_ns_is_defensively_unplaced() {
        let class = HostClass::Bare { namespace: "evil" };
        assert_eq!(
            decide_cluster_placement(class, false, true, ForeignSnapshots::Fallback, None),
            PlacementDecision::Unplaced
        );
    }

    #[test]
    fn row4_bare_disallowed_ignore_cluster_mode_on_is_foreign_ignored() {
        let class = HostClass::Bare { namespace: "ghost" };
        // "any" fallback: even a configured fallback doesn't rescue it under Ignore.
        for fallback in [None, Some("kopia-system")] {
            assert_eq!(
                decide_cluster_placement(class, false, true, ForeignSnapshots::Ignore, fallback),
                PlacementDecision::ForeignIgnored,
                "fallback={fallback:?}"
            );
        }
    }

    #[test]
    fn row5_bare_disallowed_ignore_cluster_mode_off_falls_back_exactly_like_today() {
        // Back-compat: cluster identity off is exactly the pre-M4 `placement_namespace`
        // behavior (migrated from cluster_repository.rs's
        // `disallowed_namespace_falls_back`), regardless of what `foreign` happens to be
        // (the validator prevents `foreignSnapshots` being set with no cluster identity,
        // so it is always the default `Ignore` in practice — but the decision must not
        // depend on that, only on `cluster_mode`).
        let class = HostClass::Bare { namespace: "evil" };
        assert_eq!(
            decide_cluster_placement(
                class,
                false,
                false,
                ForeignSnapshots::Ignore,
                Some("kopia-system")
            ),
            PlacementDecision::Place("kopia-system".to_string())
        );
    }

    #[test]
    fn row6_bare_disallowed_ignore_cluster_mode_off_no_fallback_is_unplaced_exactly_like_today() {
        // Migrated from cluster_repository.rs's `disallowed_and_no_fallback_yields_none`.
        let class = HostClass::Bare { namespace: "evil" };
        assert_eq!(
            decide_cluster_placement(class, false, false, ForeignSnapshots::Ignore, None),
            PlacementDecision::Unplaced
        );
    }

    #[test]
    fn row1_allowed_namespace_is_used_directly_back_compat() {
        // Migrated from cluster_repository.rs's `allowed_namespace_is_used_directly`.
        let class = HostClass::Bare {
            namespace: "billing",
        };
        assert_eq!(
            decide_cluster_placement(
                class,
                true,
                false,
                ForeignSnapshots::Ignore,
                Some("kopia-system")
            ),
            PlacementDecision::Place("billing".to_string())
        );
    }

    #[test]
    fn row7_own_cluster_allowed_places_into_the_candidate_namespace() {
        let class = HostClass::OwnCluster { namespace: "prod" };
        for foreign in [ForeignSnapshots::Ignore, ForeignSnapshots::Fallback] {
            assert_eq!(
                decide_cluster_placement(class, true, true, foreign, Some("fallback-ns")),
                PlacementDecision::Place("prod".to_string())
            );
        }
    }

    #[test]
    fn row8_own_cluster_disallowed_falls_back_regardless_of_foreign_policy() {
        let class = HostClass::OwnCluster { namespace: "prod" };
        for foreign in [ForeignSnapshots::Ignore, ForeignSnapshots::Fallback] {
            assert_eq!(
                decide_cluster_placement(class, false, true, foreign, Some("kopia-system")),
                PlacementDecision::Place("kopia-system".to_string())
            );
        }
    }

    #[test]
    fn row9_own_cluster_disallowed_no_fallback_is_unplaced() {
        let class = HostClass::OwnCluster { namespace: "prod" };
        assert_eq!(
            decide_cluster_placement(class, false, true, ForeignSnapshots::Ignore, None),
            PlacementDecision::Unplaced
        );
    }

    #[test]
    fn row10_foreign_cluster_ignore_is_foreign_ignored_regardless_of_ns_allowed_or_fallback() {
        let class = HostClass::ForeignCluster { suffix: "west" };
        for ns_allowed in [false, true] {
            for fallback in [None, Some("kopia-system")] {
                assert_eq!(
                    decide_cluster_placement(
                        class,
                        ns_allowed,
                        true,
                        ForeignSnapshots::Ignore,
                        fallback
                    ),
                    PlacementDecision::ForeignIgnored,
                    "ns_allowed={ns_allowed} fallback={fallback:?}"
                );
            }
        }
    }

    #[test]
    fn row11_foreign_cluster_fallback_places_into_the_fallback_namespace() {
        let class = HostClass::ForeignCluster { suffix: "west" };
        assert_eq!(
            decide_cluster_placement(
                class,
                false,
                true,
                ForeignSnapshots::Fallback,
                Some("kopia-system")
            ),
            PlacementDecision::Place("kopia-system".to_string())
        );
    }

    #[test]
    fn row12_foreign_cluster_fallback_no_fallback_ns_is_defensively_unplaced() {
        let class = HostClass::ForeignCluster { suffix: "west" };
        assert_eq!(
            decide_cluster_placement(class, false, true, ForeignSnapshots::Fallback, None),
            PlacementDecision::Unplaced
        );
    }

    #[test]
    fn namespaced_placement_is_byte_identical_to_today_when_cluster_is_none() {
        // A namespaced Repository with no identityDefaults.cluster set (the
        // overwhelming common case): `cluster` is None, so classify_hostname is
        // total-Bare regardless of hostname shape (even one that LOOKS like
        // <namespace>.<cluster>) — the repository always materializes into its own
        // namespace exactly as before M5 gave Repository an identityDefaults field.
        for host in ["billing", "billing.east", "ns.", ".east", ""] {
            let class = classify_hostname(host, None);
            assert_eq!(
                decide_namespace_placement(class, "prod"),
                PlacementDecision::Place("prod".to_string()),
                "host={host:?}"
            );
        }
    }

    #[test]
    fn namespaced_placement_ignores_foreign_cluster_defensively() {
        // Defensive backstop kept exhaustive for `HostClass::ForeignCluster`: never
        // materialize a foreign entry into the repo's own namespace, even if a
        // Fallback policy value somehow reached here (the validator rejects
        // Fallback on a namespaced Repository — see rule (c) — so this never fires
        // in practice, but the match stays total).
        let class = classify_hostname("billing.west", Some("east"));
        assert_eq!(
            decide_namespace_placement(class, "prod"),
            PlacementDecision::ForeignIgnored
        );
    }

    // --- plan_placements: the placement pass, with a stubbed ns_allowed resolver ---

    #[test]
    fn placement_pass_over_a_mixed_listing_yields_correct_ignore_set_count_and_decisions() {
        let listing = vec![
            entry("own1", ("u", "billing.east", "/p"), t(5)), // OwnCluster, allowed
            entry("foreign1", ("u", "billing.west", "/p"), t(5)), // ForeignCluster
            entry("bareown1", ("u", "checkout", "/p"), t(5)), // Bare, allowed
            entry("bareunknown1", ("u", "ghost", "/p"), t(5)), // Bare, disallowed
        ];
        let mut ns_allowed = BTreeMap::new();
        ns_allowed.insert("billing".to_string(), true);
        ns_allowed.insert("checkout".to_string(), true);
        ns_allowed.insert("ghost".to_string(), false);
        let allowed = AllowedNamespaces::List(vec!["billing".into(), "checkout".into()]);
        let placement = Placement::Cluster {
            allowed: &allowed,
            fallback: None,
        };
        let pass = plan_placements(
            &listing,
            Some("east"),
            ForeignSnapshots::Ignore,
            &placement,
            &ns_allowed,
        );

        assert_eq!(
            pass.decisions.get("billing.east"),
            Some(&PlacementDecision::Place("billing".to_string()))
        );
        assert_eq!(
            pass.decisions.get("billing.west"),
            Some(&PlacementDecision::ForeignIgnored)
        );
        assert_eq!(
            pass.decisions.get("checkout"),
            Some(&PlacementDecision::Place("checkout".to_string()))
        );
        // cluster mode on, ns disallowed, Ignore → treated the same as ForeignCluster.
        assert_eq!(
            pass.decisions.get("ghost"),
            Some(&PlacementDecision::ForeignIgnored)
        );

        assert_eq!(
            pass.foreign_ignored_ids,
            ["foreign1".to_string(), "bareunknown1".to_string()].into()
        );
        assert_eq!(pass.foreign_count, 2);
        assert_eq!(pass.foreign_suffix_counts.get("west"), Some(&1));
        // The bare-unknown host has no suffix, so it's keyed by the full hostname.
        assert_eq!(pass.foreign_suffix_counts.get("ghost"), Some(&1));
    }

    #[test]
    fn placement_pass_over_the_namespace_kind_never_does_cluster_ios_or_looks_at_ns_allowed() {
        let listing = vec![
            entry("a", ("u", "checkout", "/p"), t(5)),
            entry("b", ("u", "billing.west", "/p"), t(5)),
        ];
        let placement = Placement::Namespace("prod");
        // Empty ns_allowed map: the Namespace placement kind must never consult it.
        let pass = plan_placements(
            &listing,
            Some("east"),
            ForeignSnapshots::Ignore,
            &placement,
            &BTreeMap::new(),
        );
        assert_eq!(
            pass.decisions.get("checkout"),
            Some(&PlacementDecision::Place("prod".to_string()))
        );
        assert_eq!(
            pass.decisions.get("billing.west"),
            Some(&PlacementDecision::ForeignIgnored)
        );
    }

    #[test]
    fn namespaced_placement_pass_with_cluster_identity_places_own_and_bare_ignores_foreign() {
        // M5: a namespaced Repository with identityDefaults.cluster set gets the
        // SAME cluster-aware placement pass a ClusterRepository does — own-cluster
        // and bare hostnames still land in the repo's own namespace; a
        // foreign-cluster hostname is ignored (never materialized) but still
        // counted (status.catalog.foreignSnapshotCount).
        let listing = vec![
            entry("own1", ("u", "prod.east", "/p"), t(5)), // OwnCluster
            entry("bare1", ("u", "legacy", "/p"), t(5)),   // Bare (pre-cluster-identity hostname)
            entry("foreign1", ("u", "prod.west", "/p"), t(5)), // ForeignCluster
        ];
        let placement = Placement::Namespace("prod");
        let pass = plan_placements(
            &listing,
            Some("east"),
            ForeignSnapshots::Ignore,
            &placement,
            &BTreeMap::new(),
        );
        assert_eq!(
            pass.decisions.get("prod.east"),
            Some(&PlacementDecision::Place("prod".to_string())),
            "OwnCluster host must place into the repo's own namespace"
        );
        assert_eq!(
            pass.decisions.get("legacy"),
            Some(&PlacementDecision::Place("prod".to_string())),
            "Bare host must place into the repo's own namespace"
        );
        assert_eq!(
            pass.decisions.get("prod.west"),
            Some(&PlacementDecision::ForeignIgnored),
            "ForeignCluster host must be ignored, never materialized"
        );
        assert_eq!(pass.foreign_ignored_ids, ["foreign1".to_string()].into());
        assert_eq!(pass.foreign_count, 1, "the foreign entry is still counted");
        assert_eq!(pass.foreign_suffix_counts.get("west"), Some(&1));
    }

    // --- plan_catalog: foreign_ignored_ids interaction ---

    #[test]
    fn foreign_ignored_ids_are_excluded_from_creation() {
        let listing = vec![
            entry("own", ("u", "billing", "/p"), t(5)),
            entry("foreign", ("u", "billing.west", "/p"), t(3)),
        ];
        let foreign_ignored: BTreeSet<String> = ["foreign".to_string()].into();
        let plan = plan_catalog(
            &[],
            &BTreeSet::new(),
            &foreign_ignored,
            &listing,
            false,
            None,
            Utc::now(),
        );
        assert_eq!(ids(&plan), vec!["own"]);
    }

    #[test]
    fn foreign_ignored_ids_pre_existing_rows_expire_on_complete_listings() {
        let listing = vec![entry("foreign", ("u", "billing.west", "/p"), t(5))];
        let rows = vec![row("r-foreign", "foreign", Some(t(5)))];
        let foreign_ignored: BTreeSet<String> = ["foreign".to_string()].into();
        let plan = plan_catalog(
            &rows,
            &BTreeSet::new(),
            &foreign_ignored,
            &listing,
            false,
            None,
            Utc::now(),
        );
        assert!(plan.create.is_empty());
        assert_eq!(expired(&plan), vec!["r-foreign"]);
    }

    #[test]
    fn foreign_ignored_rows_still_respect_the_general_absence_expiry_rule_under_truncation() {
        // The general absence-expiry rule (skipped under a truncated listing) must
        // still hold for a row whose snapshot doesn't appear in `listing` at all this
        // scan (e.g. it was already dropped by the mover's foreign-suffix prefilter
        // before `listing`/`foreign_ignored_ids` were even built) — a plain absence
        // case, unaffected by adding the new parameter.
        let rows = vec![row("r-gone", "gone", Some(t(10)))];
        let listing = vec![entry("still", ("u", "h", "/p"), t(5))];
        let plan = plan_catalog(
            &rows,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &listing,
            true,
            None,
            Utc::now(),
        );
        assert!(
            plan.expire.is_empty(),
            "absence expiry must be skipped under truncation"
        );
    }

    #[test]
    fn foreign_ignored_entries_never_consume_the_per_identity_cap() {
        // Identity "a" has 3 eligible entries and cap=2; "a3" is foreign-ignored, so
        // it must not occupy a keep-slot — the two REMAINING entries both survive.
        let listing = vec![
            entry("a1", ("u", "a", "/p"), t(30)),
            entry("a2", ("u", "a", "/p"), t(20)),
            entry("a3", ("u", "a", "/p"), t(10)),
        ];
        let foreign_ignored: BTreeSet<String> = ["a3".to_string()].into();
        let retain = CatalogRetain {
            per_identity: Some(2),
            max_age_days: None,
        };
        let plan = plan_catalog(
            &[],
            &BTreeSet::new(),
            &foreign_ignored,
            &listing,
            false,
            Some(&retain),
            Utc::now(),
        );
        let mut got = ids(&plan);
        got.sort();
        assert_eq!(got, vec!["a1", "a2"]);
    }

    #[test]
    fn fallback_placed_foreign_entries_still_consume_caps() {
        // A Fallback-placed foreign entry is NOT in foreign_ignored_ids (it
        // materializes), so it competes for the per-identity cap like any entry.
        let listing = vec![
            entry("a1", ("u", "a", "/p"), t(30)),
            entry("a2", ("u", "a", "/p"), t(20)),
            entry("a3", ("u", "a", "/p"), t(10)),
        ];
        let retain = CatalogRetain {
            per_identity: Some(2),
            max_age_days: None,
        };
        let plan = plan_catalog(
            &[],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &listing,
            false,
            Some(&retain),
            Utc::now(),
        );
        let mut got = ids(&plan);
        got.sort();
        assert_eq!(got, vec!["a2", "a3"]);
    }
}
