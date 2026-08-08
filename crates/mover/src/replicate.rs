//! Snapshot-replication (logical, `kopia snapshot migrate`) support: the
//! **pure** selection / post-verify / naming / prune kernels, the pure copy-CR
//! builder, and the thin kube executors that reconcile dest-side
//! `origin: replicated` `Snapshot` CRs.
//!
//! Everything decision-shaped here is a pure function over plain data so it is
//! unit-testable without a cluster or a kopia binary; the kube IO
//! ([`reconcile_copy_crs`], [`prune_copy_crs`]) only executes what the pure
//! layer decided, with bounded concurrency and 404/409 tolerance so a retried
//! Job converges instead of wedging.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use kopiur_api::common::{
    DeletionPolicy, RepositoryKind, RepositoryRef, ResolvedIdentity, Retention,
};
use kopiur_api::consts::{
    ORIGIN_LABEL, PRUNED_BY_ANNOTATION, REPOSITORY_UID_LABEL, SNAPSHOT_ID_LABEL,
    SNAPSHOT_REPLICATION_LABEL,
};
use kopiur_api::retention::{SnapshotLike, select_kept};
use kopiur_api::snapshot::{
    CopiedFrom, Origin, PrunedBy, ResolvedSnapshot, Snapshot, SnapshotInfo, SnapshotPhase,
    SnapshotSpec, SnapshotStatus,
};
use kopiur_api::{SnapshotStats, SnapshotTiming};
use kopiur_kopia::SnapshotListEntry;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use tracing::{info, warn};

use crate::workspec::{
    IdentityMatcherSpec, PruningSpec, ReplicationRepositoryRef, ReplicationSourceRef,
};

/// The field-manager for every server-side apply the replication mover
/// performs on copy `Snapshot` CRs. Distinct from the controller's manager so
/// SSA ownership shows who stamped what.
pub const FIELD_MANAGER: &str = "kopiur.home-operations.com/snapshot-replication-mover";

/// How many copy-CR reconciliations / prune deletes run concurrently. Bounded
/// so a thousand-manifest first-full-history run cannot stampede the
/// apiserver; resumability (SSA idempotence + full-correspondence re-runs)
/// covers the tail if the Job dies mid-wave.
pub const COPY_SYNC_CONCURRENCY: usize = 8;

/// A kopia source identity as the structured triple `(username, hostname,
/// source_path)` — matching is ALWAYS on the triple, never on a rendered
/// string (a `@`/`:` inside a component must not confuse selection).
pub type IdentityTriple = (String, String, String);

/// The idempotency key `kopia snapshot migrate` copies under: the identity
/// triple plus the (preserved) `startTime`.
pub type SnapKey = (IdentityTriple, DateTime<Utc>);

/// The structured triple of a listing entry.
pub fn entry_triple(e: &SnapshotListEntry) -> IdentityTriple {
    (
        e.source.user_name.clone(),
        e.source.host.clone(),
        e.source.path.clone(),
    )
}

/// Render a triple as kopia's `username@hostname:path` source spec (the form
/// `snapshot migrate --sources` matches against).
pub fn triple_spec(t: &IdentityTriple) -> String {
    format!("{}@{}:{}", t.0, t.1, t.2)
}

/// Component-glob match: `*` and `?` match within a path component but never
/// across `/`; every `/` in the pattern must align with a `/` in the value.
/// Implemented by splitting both sides on `/` and glob-matching per segment —
/// obviously-correct segment semantics rather than a backtracker with a
/// non-crossing special case. For `username`/`hostname` (which contain no
/// `/`) this degenerates to a plain glob.
///
/// TODO(orchestrator): the M3 api slice exports the same matcher for the
/// webhook's glob-syntax validation; dedupe onto the api's once merged.
pub fn component_glob_matches(pattern: &str, value: &str) -> bool {
    let ps: Vec<&str> = pattern.split('/').collect();
    let vs: Vec<&str> = value.split('/').collect();
    ps.len() == vs.len() && ps.iter().zip(vs.iter()).all(|(p, v)| segment_glob(p, v))
}

/// Classic greedy glob over a single path segment (`*` = any run of chars,
/// `?` = any one char; no `/` can appear in either side here).
fn segment_glob(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    let (mut pi, mut vi) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while vi < v.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == v[vi]) && p[pi] != '*' {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, vi));
            pi += 1;
        } else if let Some((sp, sv)) = star {
            pi = sp + 1;
            vi = sv + 1;
            star = Some((sp, sv + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Whether one matcher matches a triple: every PRESENT field must
/// component-glob-match its component; an absent field matches anything. An
/// all-absent matcher (webhook-refused upstream) defensively matches NOTHING
/// — an invalid matcher must never select (or exclude) everything.
pub fn matcher_matches(m: &IdentityMatcherSpec, t: &IdentityTriple) -> bool {
    if m.username.is_none() && m.hostname.is_none() && m.source_path.is_none() {
        return false;
    }
    m.username
        .as_deref()
        .is_none_or(|p| component_glob_matches(p, &t.0))
        && m.hostname
            .as_deref()
            .is_none_or(|p| component_glob_matches(p, &t.1))
        && m.source_path
            .as_deref()
            .is_none_or(|p| component_glob_matches(p, &t.2))
}

/// Select the identities a replication run covers: an identity is selected
/// when it matches ANY include matcher (an empty include list includes every
/// identity) and NO exclude matcher (exclude always wins). Pure over the
/// listing entries' structured triples.
pub fn select_identities(
    include: &[IdentityMatcherSpec],
    exclude: &[IdentityMatcherSpec],
    entries: &[SnapshotListEntry],
) -> BTreeSet<IdentityTriple> {
    entries
        .iter()
        .map(entry_triple)
        .filter(|t| {
            let included = include.is_empty() || include.iter().any(|m| matcher_matches(m, t));
            included && !exclude.iter().any(|m| matcher_matches(m, t))
        })
        .collect()
}

/// The `(triple, startTime)` keys of `entries` restricted to `selected`
/// identities. When `latest_only`, only the NEWEST startTime per identity is
/// kept — mirroring what `snapshot migrate --latest-only` copies.
///
/// Incomplete checkpoint snapshots never appear here: kopia's `snapshot list
/// --json` omits them unless `--incomplete` is passed, and
/// [`kopiur_kopia::KopiaClient::snapshot_list_all`] never passes it — so the
/// listing itself is the complete-only set.
pub fn expected_keys(
    entries: &[SnapshotListEntry],
    selected: &BTreeSet<IdentityTriple>,
    latest_only: bool,
) -> BTreeSet<SnapKey> {
    let mut all: BTreeSet<SnapKey> = entries
        .iter()
        .filter(|e| selected.contains(&entry_triple(e)))
        .map(|e| (entry_triple(e), e.start_time))
        .collect();
    if latest_only {
        let mut newest: BTreeMap<IdentityTriple, DateTime<Utc>> = BTreeMap::new();
        for (t, s) in &all {
            let e = newest.entry(t.clone()).or_insert(*s);
            if *s > *e {
                *e = *s;
            }
        }
        all.retain(|(t, s)| newest.get(t) == Some(s));
    }
    all
}

/// The `(triple, startTime)` keys of a listing, unrestricted — the
/// mirror-source correlation set uses the source's FULL key set, so a copy
/// row is never deleted merely because the selection was narrowed while its
/// snapshot still exists on the source.
pub fn all_keys(entries: &[SnapshotListEntry]) -> BTreeSet<SnapKey> {
    entries
        .iter()
        .map(|e| (entry_triple(e), e.start_time))
        .collect()
}

/// The keys of a destination listing, restricted to `selected` identities.
pub fn dest_keys(
    entries: &[SnapshotListEntry],
    selected: &BTreeSet<IdentityTriple>,
) -> BTreeSet<SnapKey> {
    entries
        .iter()
        .filter(|e| selected.contains(&entry_triple(e)))
        .map(|e| (entry_triple(e), e.start_time))
        .collect()
}

/// The mandatory post-verify: every expected `(identity, startTime)` pair the
/// selection implied (see [`expected_keys`]) that is NOT present on the
/// destination after the migrate. kopia exits 0 even when a per-source
/// migration failed (its goroutines only log), so a non-empty result is the
/// REAL failure signal. Sorted for deterministic messages.
pub fn missing_after_migrate(
    source: &[SnapshotListEntry],
    selected: &BTreeSet<IdentityTriple>,
    dest_after: &[SnapshotListEntry],
    latest_only: bool,
) -> Vec<SnapKey> {
    let expected = expected_keys(source, selected, latest_only);
    let have = dest_keys(dest_after, selected);
    expected.difference(&have).cloned().collect()
}

/// A capped, human-readable `identity@startTime` sample list for the
/// [`crate::error::MoverError::MigrateIncomplete`] message.
pub fn missing_sample(missing: &[SnapKey], cap: usize) -> String {
    missing
        .iter()
        .take(cap)
        .map(|(t, s)| format!("{}@{}", triple_spec(t), s.to_rfc3339()))
        .collect::<Vec<_>>()
        .join(", ")
}

// --- copy-CR naming + builder (pure) ---------------------------------------

/// The name the dest-side copy `Snapshot` CR for `dest_manifest_id` gets:
/// `<replication>-copy-<first16(id)>-<hash8(full id)>`, length-capped —
/// mirroring the adoption naming pattern (`adopted_cr_name`): the first-16
/// prefix keeps human correlation with the kopia id, the trailing hash of the
/// FULL id keeps names distinct for ids sharing a ≥16-char prefix, and the
/// determinism is what makes the SSA reconciliation resumable.
pub fn copy_cr_name(repl_name: &str, dest_manifest_id: &str) -> String {
    let short: String = dest_manifest_id.chars().take(16).collect();
    let hash = short_hash(dest_manifest_id);
    capped_name(&format!("{repl_name}-copy-{short}-{hash}"))
}

/// Cap a generated name at Kubernetes' 63-char limit, keeping a stable prefix
/// and appending a 16-hex FNV-1a of the FULL name for uniqueness.
///
/// TODO(orchestrator): byte-identical to `kopiur_controller::naming::capped_name`
/// (and `short_hash`/`fnv1a` below); dedupe into a shared home at wave end.
fn capped_name(full: &str) -> String {
    if full.len() <= 63 {
        return full.to_string();
    }
    let prefix: String = full.chars().take(46).collect();
    format!("{}-{:016x}", prefix.trim_end_matches('-'), fnv1a(full))
}

/// A short, stable 8-hex-char FNV-1a hash for name disambiguation.
fn short_hash(s: &str) -> String {
    format!("{:08x}", (fnv1a(s) & 0xffff_ffff))
}

/// 64-bit FNV-1a over the string's bytes.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Parse the wire `kind` string back to the api enum. The controller writes it
/// from `RepositoryKind`'s serde value, so both sides agree within one
/// operator version; anything unrecognized falls back to the namespaced
/// `Repository` (the enum's own serde default) rather than failing a run that
/// already copied data.
fn parse_repo_kind(kind: &str) -> RepositoryKind {
    match kind {
        "ClusterRepository" => RepositoryKind::ClusterRepository,
        _ => RepositoryKind::Repository,
    }
}

/// The api `RepositoryRef` for the wire destination block.
pub fn dest_repository_ref(dest: &ReplicationRepositoryRef) -> RepositoryRef {
    RepositoryRef {
        kind: parse_repo_kind(&dest.kind),
        name: dest.name.clone(),
        namespace: dest.namespace.clone(),
    }
}

/// The api `RepositoryRef` for the wire source block.
pub fn source_repository_ref(source: &ReplicationSourceRef) -> RepositoryRef {
    RepositoryRef {
        kind: parse_repo_kind(&source.kind),
        name: source.name.clone(),
        namespace: source.namespace.clone(),
    }
}

/// A foreign-writer-controlled description, truncated to 1024 bytes on a char
/// boundary (the same rule discovered rows apply) — it must never fail the CR
/// write. Empty ⇒ elided.
fn truncated_description(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    const MAX: usize = 1024;
    if s.len() <= MAX {
        return Some(s.to_string());
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    Some(s[..end].to_string())
}

/// Build the dest-side copy `Snapshot` CR (spec + metadata) and its status for
/// one migrated manifest, in the replication CR's namespace. **Pure.** The
/// status is returned separately because create/apply strips `.status` — the
/// caller applies the CR, then PATCHes the status subresource with ONE atomic
/// body (kopiaSnapshotID + resolved.repository + copiedFrom together, closing
/// the unpinned breaker/deletion window).
///
/// - **Labels at birth**: `origin: replicated`, `SNAPSHOT_ID_LABEL` = the
///   DESTINATION manifest id, `REPOSITORY_UID_LABEL` = the destination repo
///   CR's uid, `SNAPSHOT_REPLICATION_LABEL` = the replication CR's name (the
///   three-label conjunction is the pruning candidate set).
/// - **Spec**: `repository` = the destination ref stamped at create (the
///   cross-feature contract), `policyRef: None` (never a policy child),
///   `deletionPolicy: Delete` (the CR owns its migrated manifest).
/// - **Meta**: NO ownerReferences — deleting the SnapshotReplication never
///   deletes copies.
pub fn build_copy_snapshot(
    repl_name: &str,
    namespace: &str,
    dest_repo: &ReplicationRepositoryRef,
    source_repo: &ReplicationSourceRef,
    dest_entry: &SnapshotListEntry,
    source_manifest_id: &str,
) -> (Snapshot, SnapshotStatus) {
    let cr_name = copy_cr_name(repl_name, &dest_entry.id);
    let dest_ref = dest_repository_ref(dest_repo);

    let mut labels = BTreeMap::new();
    labels.insert(
        ORIGIN_LABEL.to_string(),
        Origin::Replicated.label_value().to_string(),
    );
    labels.insert(SNAPSHOT_ID_LABEL.to_string(), dest_entry.id.clone());
    labels.insert(REPOSITORY_UID_LABEL.to_string(), dest_repo.uid.clone());
    labels.insert(
        SNAPSHOT_REPLICATION_LABEL.to_string(),
        repl_name.to_string(),
    );

    let mut snapshot = Snapshot::new(
        &cr_name,
        SnapshotSpec {
            policy_ref: None,
            repository: Some(dest_ref.clone()),
            source: None,
            tags: None,
            failure_policy: None,
            deletion_policy: Some(DeletionPolicy::Delete),
            on_schedule_delete: None,
            pin: false,
            description: None,
        },
    );
    snapshot.metadata.name = Some(cr_name);
    snapshot.metadata.namespace = Some(namespace.to_string());
    snapshot.metadata.labels = Some(labels);
    // NO ownerReferences: deleting the SnapshotReplication CR never deletes
    // the copies (they are catalog history, owned by their own finalizer).

    let identity = ResolvedIdentity {
        username: dest_entry.source.user_name.clone(),
        hostname: dest_entry.source.host.clone(),
        source_path: Some(dest_entry.source.path.clone()),
    };
    let size = dest_entry.stats.total_size;
    let status = SnapshotStatus {
        phase: Some(SnapshotPhase::Succeeded),
        origin: Some(Origin::Replicated),
        snapshot: Some(SnapshotInfo {
            kopia_snapshot_id: dest_entry.id.clone(),
            identity,
            description: truncated_description(&dest_entry.description),
        }),
        timing: Some(SnapshotTiming {
            start_time: Some(dest_entry.start_time.to_rfc3339()),
            end_time: Some(dest_entry.end_time.to_rfc3339()),
            duration_seconds: Some((dest_entry.end_time - dest_entry.start_time).num_seconds()),
        }),
        stats: (size > 0).then(|| SnapshotStats {
            size_bytes: Some(i64::try_from(size).unwrap_or(i64::MAX)),
            ..Default::default()
        }),
        resolved: Some(ResolvedSnapshot {
            repository: Some(dest_ref),
            sources: Vec::new(),
            credential_projection: None,
        }),
        copied_from: Some(CopiedFrom {
            repository: source_repository_ref(source_repo),
            source_manifest_id: source_manifest_id.to_string(),
            start_time: dest_entry.start_time.to_rfc3339(),
        }),
        ..Default::default()
    };
    (snapshot, status)
}

// --- pruning selection (pure) -----------------------------------------------

/// The prune-relevant view of one copy CR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyRow {
    /// The CR name (what a prune deletes).
    pub name: String,
    /// The identity triple from `status.snapshot.identity`.
    pub identity: IdentityTriple,
    /// Parsed `status.timing.startTime` — the mirror-source correlation key.
    pub start_time: Option<DateTime<Utc>>,
    /// Parsed `status.timing.endTime` — the GFS bucketing key.
    pub end_time: Option<DateTime<Utc>>,
    /// `spec.pin`: pinned rows are exempt from EVERY mover-initiated prune.
    pub pinned: bool,
}

/// Extract a [`CopyRow`] from a listed copy CR. `None` when the row carries no
/// usable identity — such a row is conservatively never pruned.
pub fn copy_row_from_snapshot(snap: &Snapshot) -> Option<CopyRow> {
    use kube::ResourceExt;
    let status = snap.status.as_ref()?;
    let info = status.snapshot.as_ref()?;
    let source_path = info.identity.source_path.clone()?;
    let parse = |s: &Option<String>| {
        s.as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc))
    };
    let timing = status.timing.as_ref();
    Some(CopyRow {
        name: snap.name_any(),
        identity: (
            info.identity.username.clone(),
            info.identity.hostname.clone(),
            source_path,
        ),
        start_time: timing.and_then(|t| parse(&t.start_time)),
        end_time: timing.and_then(|t| parse(&t.end_time)),
        pinned: snap.spec.pin,
    })
}

/// A [`SnapshotLike`] view over a [`CopyRow`] with a resolved end time, so the
/// prune selection runs through the api crate's GFS kernel.
struct RetentionRow<'a> {
    row: &'a CopyRow,
    end: DateTime<Utc>,
}

impl SnapshotLike for RetentionRow<'_> {
    fn end_time(&self) -> DateTime<Utc> {
        self.end
    }
    fn id(&self) -> &str {
        &self.row.name
    }
    fn pinned(&self) -> bool {
        self.row.pinned
    }
}

/// `pruning: retention` selection: bucket the copy rows PER IDENTITY and run
/// the shared GFS kernel ([`kopiur_api::select_kept`]) over each bucket —
/// `keepDaily: 7` keeps 7 per identity, exactly like policy retention. Rows
/// with no parseable end time are conservatively kept (never delete on
/// missing data). Returns the CR names to delete, sorted.
pub fn retention_prune_names(rows: &[CopyRow], retention: &Retention) -> Vec<String> {
    let mut buckets: BTreeMap<&IdentityTriple, Vec<RetentionRow<'_>>> = BTreeMap::new();
    for row in rows {
        let Some(end) = row.end_time else {
            continue; // no bucketing key ⇒ never prune it
        };
        buckets
            .entry(&row.identity)
            .or_default()
            .push(RetentionRow { row, end });
    }
    let mut delete: Vec<String> = buckets
        .values()
        .flat_map(|bucket| select_kept(bucket, retention).delete)
        .collect();
    delete.sort();
    delete
}

/// `pruning: mirrorSource` selection: copy rows whose `(identity, startTime)`
/// is ABSENT from the source's complete-snapshot key set. Rows with no
/// parseable start time are conservatively kept; pinned rows are exempt like
/// every mover-initiated prune. Returns the CR names to delete, sorted.
pub fn mirror_prune_names(rows: &[CopyRow], source_keys: &BTreeSet<SnapKey>) -> Vec<String> {
    let mut delete: Vec<String> = rows
        .iter()
        .filter(|r| !r.pinned)
        .filter_map(|r| {
            let start = r.start_time?;
            (!source_keys.contains(&(r.identity.clone(), start))).then(|| r.name.clone())
        })
        .collect();
    delete.sort();
    delete
}

// --- kube executors ----------------------------------------------------------

/// One entry of the full correspondence set: a destination manifest of a
/// selected identity that is present in the source set, plus the SOURCE
/// manifest id it corresponds to (matched on `(identity, startTime)`).
#[derive(Debug, Clone)]
pub struct CopyCorrespondence {
    /// The destination listing entry.
    pub dest_entry: SnapshotListEntry,
    /// The source manifest id for `status.copiedFrom.sourceManifestId`.
    pub source_manifest_id: String,
}

/// Build the full correspondence set from the dest-after listing: every dest
/// manifest whose identity is selected AND whose `(identity, startTime)`
/// exists in the source listing (matched back to the source manifest id).
/// Pure. Runs over the FULL set every run — not the just-migrated delta — so a
/// mover that died between migrate and CR creation heals on the next run.
pub fn correspondence_set(
    source: &[SnapshotListEntry],
    selected: &BTreeSet<IdentityTriple>,
    dest_after: &[SnapshotListEntry],
) -> Vec<CopyCorrespondence> {
    let source_ids: BTreeMap<SnapKey, &str> = source
        .iter()
        .map(|e| ((entry_triple(e), e.start_time), e.id.as_str()))
        .collect();
    dest_after
        .iter()
        .filter(|e| selected.contains(&entry_triple(e)))
        .filter_map(|e| {
            let src = source_ids.get(&(entry_triple(e), e.start_time))?;
            Some(CopyCorrespondence {
                dest_entry: e.clone(),
                source_manifest_id: (*src).to_string(),
            })
        })
        .collect()
}

/// Outcome counters of a copy-CR reconciliation wave.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopySyncOutcome {
    /// Correspondence entries whose copy CR now exists (created or confirmed).
    pub ensured: usize,
    /// Entries whose reconciliation failed (kube errors; retried next run).
    pub failed: usize,
    /// The correspondence set's size.
    pub total: usize,
}

/// Whether a kube error is tolerable during the reconciliation: a 404 (the
/// object is already gone) or a 409 (a concurrent writer got there first) —
/// both mean the goal state is being reached by someone, so the wave must not
/// fail on them.
fn tolerable(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(e) if e.code == 404 || e.code == 409)
}

/// Reconcile the dest-side copy `Snapshot` CRs for the FULL correspondence
/// set, with bounded concurrency ([`COPY_SYNC_CONCURRENCY`]).
///
/// Per entry, in the adoption-invariant order:
/// 1. server-side APPLY the copy CR (labels at birth + spec pin; idempotent
///    resume) — skipped when a non-`discovered` CR already carries this
///    manifest id's label (already replicated, or a produced row);
/// 2. PATCH its status subresource with ONE atomic body;
/// 3. DELETE any `origin: discovered` duplicate (same repo-uid + snapshot-id
///    labels) in this namespace — replicated-first ordering, so the catalog
///    can never observe a gap.
pub async fn reconcile_copy_crs(
    api: &kube::Api<Snapshot>,
    repl_name: &str,
    namespace: &str,
    dest_repo: &ReplicationRepositoryRef,
    source_repo: &ReplicationSourceRef,
    correspondence: &[CopyCorrespondence],
) -> Result<CopySyncOutcome, kube::Error> {
    // One LIST up front: every Snapshot CR in this namespace carrying the dest
    // repo's uid label, keyed by its snapshot-id label. This is what "already
    // has a non-discovered CR" and "discovered duplicate" are decided from.
    let lp = ListParams::default().labels(&format!("{REPOSITORY_UID_LABEL}={}", dest_repo.uid));
    let existing = api.list(&lp).await?;
    let mut by_id: BTreeMap<String, Vec<(String, Option<Origin>)>> = BTreeMap::new();
    for snap in existing.items {
        use kube::ResourceExt;
        let labels = snap.labels();
        let Some(id) = labels.get(SNAPSHOT_ID_LABEL) else {
            continue;
        };
        let origin = labels.get(ORIGIN_LABEL).and_then(|v| Origin::parse(v));
        by_id
            .entry(id.clone())
            .or_default()
            .push((snap.name_any(), origin));
    }

    let mut join: tokio::task::JoinSet<Result<(), kube::Error>> = tokio::task::JoinSet::new();
    let mut outcome = CopySyncOutcome {
        total: correspondence.len(),
        ..Default::default()
    };
    let settle = |res: Option<Result<Result<(), kube::Error>, tokio::task::JoinError>>,
                  outcome: &mut CopySyncOutcome| {
        match res {
            Some(Ok(Ok(()))) => outcome.ensured += 1,
            Some(Ok(Err(e))) => {
                warn!(error = %e, "copy Snapshot CR reconciliation failed; continuing");
                outcome.failed += 1;
            }
            Some(Err(e)) => {
                warn!(error = %e, "copy Snapshot CR reconciliation task panicked; continuing");
                outcome.failed += 1;
            }
            None => {}
        }
    };

    for item in correspondence {
        let rows = by_id.get(&item.dest_entry.id);
        // An unparseable origin label (a NEWER operator's value) is treated as
        // non-discovered: conservatively neither replaced nor deleted.
        let replicated_exists = rows.is_some_and(|v| {
            v.iter()
                .any(|(_, origin)| !matches!(origin, Some(Origin::Discovered)))
        });
        let dups: Vec<String> = rows
            .map(|v| {
                v.iter()
                    .filter(|(_, o)| matches!(o, Some(Origin::Discovered)))
                    .map(|(n, _)| n.clone())
                    .collect()
            })
            .unwrap_or_default();
        if replicated_exists && dups.is_empty() {
            outcome.ensured += 1;
            continue;
        }

        while join.len() >= COPY_SYNC_CONCURRENCY {
            let res = join.join_next().await;
            settle(res, &mut outcome);
        }
        let api = api.clone();
        let (snap, status) = build_copy_snapshot(
            repl_name,
            namespace,
            dest_repo,
            source_repo,
            &item.dest_entry,
            &item.source_manifest_id,
        );
        join.spawn(async move {
            sync_one(&api, replicated_exists, snap, status, &dups).await?;
            Ok(())
        });
    }
    loop {
        let res = join.join_next().await;
        if res.is_none() {
            break;
        }
        settle(res, &mut outcome);
    }
    Ok(outcome)
}

/// Reconcile ONE copy CR: apply + atomic status PATCH (unless a replicated row
/// already exists), then delete any discovered duplicates. 404/409-tolerant.
async fn sync_one(
    api: &kube::Api<Snapshot>,
    replicated_exists: bool,
    snap: Snapshot,
    status: SnapshotStatus,
    dups: &[String],
) -> Result<(), kube::Error> {
    use kube::ResourceExt;
    let name = snap.name_any();
    if !replicated_exists {
        // (1) CREATE (SSA apply) with labels + the spec repository pin at
        // birth; force so a re-run after a partial failure re-takes the fields.
        let pp = PatchParams::apply(FIELD_MANAGER).force();
        api.patch(&name, &pp, &Patch::Apply(&snap)).await?;
        // (2) ONE atomic status body: phase + origin + kopiaSnapshotID +
        // resolved.repository + copiedFrom land together.
        let body = serde_json::json!({ "status": status });
        api.patch_status(&name, &PatchParams::default(), &Patch::Merge(&body))
            .await?;
        info!(snapshot = %name, "ensured replicated copy Snapshot CR");
    }
    // (3) Only after the replicated row exists: reap discovered duplicates
    // (the adoption-invariant ordering — the catalog never observes a gap).
    for dup in dups {
        match api.delete(dup, &DeleteParams::default()).await {
            Ok(_) => info!(duplicate = %dup, "deleted discovered duplicate of a replicated copy"),
            Err(e) if tolerable(&e) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Execute the carried pruning mode over the copy CRs whose labels carry ALL
/// THREE of: this replication instance, `origin: replicated`, and the
/// destination repo uid — the conjunction that makes any other row
/// structurally unreachable to this prune. Returns `(pruned, failed)` counts.
///
/// `pruning: retention` stamps `pruned-by: replication-retention` BEFORE each
/// delete (operator prune ⇒ breaker-exempt); `pruning: mirrorSource`
/// deliberately stamps NOTHING, so the deletes classify EXTERNAL and the dest
/// repository's mass-deletion breaker holds a bulk source-vanish.
pub async fn prune_copy_crs(
    api: &kube::Api<Snapshot>,
    repl_name: &str,
    dest_repo_uid: &str,
    pruning: &PruningSpec,
    source_keys: &BTreeSet<SnapKey>,
) -> Result<(usize, usize), kube::Error> {
    let (names, stamp): (Vec<String>, bool) = match pruning {
        PruningSpec::None(_) => return Ok((0, 0)),
        PruningSpec::Retention(r) => {
            let rows = list_copy_rows(api, repl_name, dest_repo_uid).await?;
            (retention_prune_names(&rows, &r.to_retention()), true)
        }
        PruningSpec::MirrorSource(_) => {
            let rows = list_copy_rows(api, repl_name, dest_repo_uid).await?;
            (mirror_prune_names(&rows, source_keys), false)
        }
    };
    let mut pruned = 0usize;
    let mut failed = 0usize;
    for name in &names {
        match prune_one(api, name, stamp).await {
            Ok(()) => pruned += 1,
            Err(e) => {
                warn!(snapshot = %name, error = %e, "copy CR prune failed; continuing");
                failed += 1;
            }
        }
    }
    Ok((pruned, failed))
}

/// LIST the three-label candidate set and extract the prune-relevant rows.
async fn list_copy_rows(
    api: &kube::Api<Snapshot>,
    repl_name: &str,
    dest_repo_uid: &str,
) -> Result<Vec<CopyRow>, kube::Error> {
    let selector = format!(
        "{SNAPSHOT_REPLICATION_LABEL}={repl_name},{ORIGIN_LABEL}={},{REPOSITORY_UID_LABEL}={dest_repo_uid}",
        Origin::Replicated.label_value(),
    );
    let list = api.list(&ListParams::default().labels(&selector)).await?;
    Ok(list
        .items
        .iter()
        .filter_map(copy_row_from_snapshot)
        .collect())
}

/// Delete one pruned copy CR, annotating `pruned-by: replication-retention`
/// first when `stamp` (retention mode) — the stamp is what classifies the
/// deletion as an operator prune (breaker-exempt); mirror-source deletes skip
/// it ON PURPOSE. 404-tolerant on both steps.
async fn prune_one(api: &kube::Api<Snapshot>, name: &str, stamp: bool) -> Result<(), kube::Error> {
    if stamp {
        let body = serde_json::json!({
            "metadata": { "annotations": {
                PRUNED_BY_ANNOTATION: PrunedBy::ReplicationRetention.annotation_value(),
            } }
        });
        match api
            .patch(name, &PatchParams::default(), &Patch::Merge(&body))
            .await
        {
            Ok(_) => {}
            Err(e) if tolerable(&e) => return Ok(()), // already gone
            Err(e) => return Err(e),
        }
    }
    match api.delete(name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(e) if tolerable(&e) => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests;
