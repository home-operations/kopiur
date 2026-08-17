//! Repository-bootstrap result types + the pure "should we create?" decision.
//!
//! The mover's `BootstrapRepository` operation connects to (or creates) an
//! object-store repository the controller cannot reach in-process (ADR §5.4) and
//! reports the outcome back via a [`BootstrapResult`] written into the work-spec
//! `ConfigMap` (key [`RESULT_CONFIGMAP_KEY`]). The controller — the single writer
//! of the `Repository` status — reads it and patches `phase`/`uniqueId`/
//! `storageStats`, then materializes `origin: discovered` Snapshot CRs.
//!
//! This module is **pure data + serde** plus the create-gate decision; the kopia
//! subprocess calls and the kube `ConfigMap` PATCH live in `main.rs`.

use kopiur_api::recorded::{
    KOPIUR_META_TAG, MetaTagDecode, decode_meta_tag, encode_meta_tag, truncate_utf8,
};
use kopiur_api::{HostClass, classify_hostname};
use kopiur_kopia::{KopiaError, KopiaErrorClass, SnapshotListEntry};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::status::{FailureBlock, failure_block_from_kopia};

/// The `ConfigMap` data key the bootstrap result is written under (the mover
/// writes it; the controller reads it — one definition so the contract can't
/// drift, mirroring [`crate::env::WORK_SPEC_PATH`]).
pub const RESULT_CONFIGMAP_KEY: &str = "result.json";

/// Upper bound on snapshot entries returned for materialization. Bounds the
/// `ConfigMap` size (etcd's ~1MB object limit). Applied AFTER
/// [`apply_foreign_prefilter`] drops another cluster's entries (when the controller
/// armed it), so a busy foreign peer's snapshots can no longer crowd this cluster's
/// own out of the capped, returned listing — see [`prepare_catalog_entries`], which
/// applies both in that order. A **bare** hostname's entries are NEVER dropped by
/// the prefilter (classifying them needs a namespace lookup only the controller can
/// do) and so still count against this cap. The snapshot *count* is reported
/// exactly regardless (not affected by either the prefilter or the cap); only the
/// per-entry list for materialization is capped, and the cap is surfaced via
/// [`BootstrapResult::snapshots_truncated`] (never a silent truncation).
pub const MAX_RETURNED_SNAPSHOTS: usize = 1000;

/// Drop listing entries whose hostname classifies
/// [`kopiur_api::HostClass::ForeignCluster`] against `prefilter_cluster` — the
/// controller's `BootstrapRepositoryOp::catalog_foreign_prefilter_cluster`, set only
/// when cluster identity is on AND the effective `catalog.foreignSnapshots` policy is
/// `Ignore`. `None` performs no filtering (repo not in cluster-`Ignore` mode) and
/// returns `listing` unchanged. A **bare** hostname (no `.`) is never dropped here
/// even under a cluster identity — classifying it needs a namespace lookup the mover
/// cannot perform; it reaches the controller and still counts against
/// [`MAX_RETURNED_SNAPSHOTS`], where the identity-aware placement pass decides its
/// fate. Pure.
pub fn apply_foreign_prefilter(
    listing: Vec<SnapshotListEntry>,
    prefilter_cluster: Option<&str>,
) -> (Vec<SnapshotListEntry>, i64) {
    let Some(cluster) = prefilter_cluster else {
        return (listing, 0);
    };
    let mut kept = Vec::with_capacity(listing.len());
    let mut dropped = 0i64;
    for entry in listing {
        match classify_hostname(&entry.source.host, Some(cluster)) {
            HostClass::ForeignCluster { .. } => dropped += 1,
            HostClass::Bare { .. } | HostClass::OwnCluster { .. } => kept.push(entry),
        }
    }
    (kept, dropped)
}

/// Prepare a raw kopia listing for materialization: [`apply_foreign_prefilter`],
/// THEN cap to [`MAX_RETURNED_SNAPSHOTS`] — that order means a busy foreign peer
/// can no longer crowd this cluster's own snapshots out of the capped list. Returns
/// `(entries, truncated, foreign_suffix_dropped)`. Pure; the sole caller is the
/// mover's `run_bootstrap` (the kopia `snapshot list` IO happens before this, not
/// within it).
pub fn prepare_catalog_entries(
    listing: Vec<SnapshotListEntry>,
    prefilter_cluster: Option<&str>,
) -> (Vec<SnapshotListEntry>, bool, i64) {
    let (mut listing, dropped) = apply_foreign_prefilter(listing, prefilter_cluster);
    let truncated = listing.len() > MAX_RETURNED_SNAPSHOTS;
    if truncated {
        listing.truncate(MAX_RETURNED_SNAPSHOTS);
    }
    // Slim each returned entry to only the fields the controller materializes.
    // The prefilter above needed `source.host`; nothing downstream needs the
    // heavy `rootEntry`/`retentionReason`, so drop them here — this is what
    // actually bounds the result ConfigMap size (see [`slim_catalog_entry`]).
    let listing = listing.into_iter().map(slim_catalog_entry).collect();
    (listing, truncated, dropped)
}

/// Byte cap on the `description` a returned entry carries on the result wire —
/// the same cap the controller applies when copying it onto the CR
/// (`catalog::DESCRIPTION_CAP_BYTES`), so the wire never carries bytes the CR
/// would drop anyway. Foreign-writer-controlled input.
pub const DESCRIPTION_WIRE_CAP_BYTES: usize = 1024;

/// Byte cap on an UNDECODABLE `kopiur-meta` value carried bounded-verbatim on
/// the wire (see [`normalize_meta_tags`]) so the controller can re-derive the
/// UnsupportedSchema/Malformed classification and aggregate-count it, without a
/// forged multi-MB tag inflating the ConfigMap.
const META_TAG_WIRE_CAP_BYTES: usize = 4096;

/// Strip the fields the controller never reads from a materialization entry,
/// keeping only what the catalog consumes (`id`, `source`, `startTime`,
/// `endTime`, `stats.totalSize`, a CAPPED `description`, and the normalized
/// `kopiur-meta` tag). Pure.
///
/// This is what actually bounds the result `ConfigMap` under etcd's ~1 MiB object
/// limit (issue #237): [`MAX_RETURNED_SNAPSHOTS`] caps the *count*, but a full
/// [`SnapshotListEntry`] serializes to ~2 KB — its `rootEntry.summ` carries an
/// **unbounded** per-file `errors` list, plus a `retentionReason` array, a
/// free-form `description`, and (foreign-writer-controlled) `tags` — so ~500
/// real-world entries already blow past 1 MiB and wedge the repository at
/// `Bootstrapped: False`. Slimmed, each entry is a few hundred bytes (+ a
/// bounded description/meta payload), so the 1000-entry cap is size-safe.
fn slim_catalog_entry(mut e: SnapshotListEntry) -> SnapshotListEntry {
    if e.description.len() > DESCRIPTION_WIRE_CAP_BYTES {
        e.description = truncate_utf8(&e.description, DESCRIPTION_WIRE_CAP_BYTES).to_string();
    }
    e.root_entry = None;
    e.retention_reason = Vec::new();
    e.tags = normalize_meta_tags(&e.tags);
    e
}

/// Normalize a raw manifest `tags` map for the result wire: raw user tags never
/// ride the ConfigMap — only the `kopiur-meta` payload survives, O(1) per entry.
///
/// - Decodable meta is RE-ENCODED canonically under the bare [`KOPIUR_META_TAG`]
///   key (compact, bounded by construction), which
///   [`kopiur_api::decode_meta_tag`] accepts on the controller side exactly like
///   the raw `tag:`-prefixed key an in-process listing carries — one decoder,
///   two wires.
/// - An undecodable value (newer schema / malformed) rides bounded-verbatim
///   ([`META_TAG_WIRE_CAP_BYTES`]) so the controller re-derives the same
///   classification and aggregate-counts it in its scan summary.
/// - Everything else (user tags, the legacy `tag:kopiur` config tag) is
///   dropped: the catalog never reads them, and they are unbounded input.
fn normalize_meta_tags(tags: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    match decode_meta_tag(tags) {
        MetaTagDecode::Absent => BTreeMap::new(),
        MetaTagDecode::Decoded(meta) => {
            BTreeMap::from([(KOPIUR_META_TAG.to_string(), encode_meta_tag(&meta))])
        }
        MetaTagDecode::UnsupportedSchema { .. } | MetaTagDecode::Malformed { .. } => {
            let raw = tags
                .get(&format!("tag:{KOPIUR_META_TAG}"))
                .or_else(|| tags.get(KOPIUR_META_TAG))
                .map(String::as_str)
                .unwrap_or_default();
            BTreeMap::from([(
                KOPIUR_META_TAG.to_string(),
                truncate_utf8(raw, META_TAG_WIRE_CAP_BYTES).to_string(),
            )])
        }
    }
}

/// Conservative byte budget for the serialized `result.json` value the mover
/// writes into the bootstrap `ConfigMap`. The apiserver rejects a ConfigMap whose
/// TOTAL size exceeds ~1 MiB (etcd's object limit); `result.json` is the only data
/// key (the work-spec rides the Job env since PR #225), so this leaves ample room
/// for the object's metadata/envelope. See [`enforce_result_size_budget`].
pub const RESULT_SIZE_BUDGET_BYTES: usize = 900 * 1024;

/// Backstop for the ConfigMap 1 MiB limit (issue #237): if the serialized `result`
/// would still exceed `budget_bytes` (e.g. pathologically long identity paths, or a
/// future field growth that [`slim_catalog_entry`] no longer covers), drop trailing
/// `snapshots` entries — already newest-first from the cap — until it fits, flagging
/// [`BootstrapResult::snapshots_truncated`] so the controller logs that not all were
/// materialized. The authoritative `snapshot_count` is left untouched. Pure and
/// deterministic; returns the (possibly trimmed) result.
pub fn enforce_result_size_budget(
    mut result: BootstrapResult,
    budget_bytes: usize,
) -> BootstrapResult {
    let serialized_len =
        |r: &BootstrapResult| serde_json::to_string(r).map(|s| s.len()).unwrap_or(0);
    // Drop trailing entries in shrinking chunks until it fits (or none remain).
    // Re-measuring per round converges quickly since entries are near-uniform size.
    while !result.snapshots.is_empty() && serialized_len(&result) > budget_bytes {
        let len = result.snapshots.len();
        let drop = (len / 8).max(1);
        result.snapshots.truncate(len.saturating_sub(drop));
        result.snapshots_truncated = true;
    }
    result
}

/// Sentinel [`FailureBlock::kopia_error_class`] the mover writes when connect found
/// **no** repository at the backend and `spec.create.enabled` is `false`, so kopiur
/// declined to initialize one. The controller keys on this exact label
/// ([`crate::bootstrap`] is its single source of truth, shared with
/// `kopiur-controller`) to surface a dedicated, actionable `RepositoryNotInitialized`
/// condition rather than a bare kopia `NotFound`.
///
/// Deliberately **not** a [`kopiur_kopia::KopiaErrorClass`]: that enum classifies
/// kopia's *stderr*, whereas "the repo is simply absent and create is opt-out" is a
/// kopiur create-*policy* outcome, not a backend error.
pub const REPOSITORY_NOT_INITIALIZED_CLASS: &str = "RepositoryNotInitialized";

/// Stable, volatile-free actionable message for the
/// [`REPOSITORY_NOT_INITIALIZED_CLASS`] case. The controller uses it verbatim as a
/// condition message, so it must carry no per-attempt detail (no temp filenames):
/// what failed, why, and the two concrete fixes.
pub const REPOSITORY_NOT_INITIALIZED_MESSAGE: &str = "no kopia repository exists at this backend (connect returned NotFound) and \
     spec.create.enabled is false, so kopiur did not initialize one; set \
     spec.create.enabled: true to create a new repository here, or point the backend \
     at an existing repository";

/// Sentinel [`FailureBlock::kopia_error_class`] the mover writes when a seed's
/// SOURCE backend answered but holds no kopia repository at all (issue #380) —
/// a connect that classified `NotFound` with kopia's "not initialized" on
/// stderr. Almost always a mis-pointed bucket/prefix or a mirror that was never
/// written.
///
/// A sibling of [`REPOSITORY_NOT_INITIALIZED_CLASS`], and deliberately NOT a
/// [`kopiur_kopia::KopiaErrorClass`], for the same reason: the class enum
/// describes kopia's stderr, while "the seed source is not a repository" is a
/// kopiur seeding-policy outcome the controller renders as its own condition
/// reason.
pub const SEED_SOURCE_NOT_FOUND_CLASS: &str = "SeedSourceNotFound";

/// Stable, volatile-free actionable message for [`SEED_SOURCE_NOT_FOUND_CLASS`].
/// Used verbatim as a condition message, so it carries no per-attempt detail.
pub const SEED_SOURCE_NOT_FOUND_MESSAGE: &str = "spec.seed's source answered but holds no kopia repository (connect returned NotFound with \
     kopia's `repository not initialized`), so there is nothing to seed this repository from. \
     Check that spec.seed.from points at the bucket AND prefix a kopia repository actually \
     lives under — a mirror written by a RepositoryReplication is rooted at the destination's \
     own prefix, not its parent. The bootstrap retries automatically once the source is \
     reachable";

/// Sentinel [`FailureBlock::kopia_error_class`] for a seed source that IS a
/// kopia repository but holds zero snapshots, with `spec.seed.allowEmptySource`
/// left at its `false` default (issue #380).
///
/// Blocking `Ready` here is the point: a valid-but-empty mirror is nearly always
/// a mis-pointed source, and seeding nothing would hand back a `Ready`
/// repository with no history — the exact failure #380 exists to prevent.
pub const SEED_SOURCE_EMPTY_CLASS: &str = "SeedSourceEmpty";

/// Stable, volatile-free actionable message for [`SEED_SOURCE_EMPTY_CLASS`],
/// naming the explicit override.
pub const SEED_SOURCE_EMPTY_MESSAGE: &str = "spec.seed's source is a kopia repository but holds zero snapshots, so seeding it would \
     leave this repository empty while reporting it Ready. Check that spec.seed.from points at \
     the intended mirror (an empty one is usually a wrong bucket/prefix or a replication that \
     never ran); if the source really is meant to be empty, set spec.seed.allowEmptySource: \
     true. The bootstrap retries automatically, so a mirror that fills up later seeds without \
     further action";

/// Sentinel [`FailureBlock::kopia_error_class`] for a migrate-mode seed where
/// `kopia snapshot migrate` exited 0 but the post-verify found snapshots
/// missing at the destination (issue #380).
///
/// Mandatory because kopia's per-source migration goroutines only LOG their
/// errors — a zero exit does NOT mean every snapshot arrived, so the
/// destination listing is the only honest success signal. Retryable: migrate is
/// idempotent by `(identity, startTime)`, so a retry copies only what is still
/// missing.
pub const SEED_INCOMPLETE_CLASS: &str = "SeedIncomplete";

/// The outcome of a seed that ran (or was skipped as already-initialized),
/// carried on [`BootstrapResult::seed`].
///
/// **Presence is the controller's mover-skew guard** (issue #380): when the work
/// spec armed a seed, a successful result WITHOUT this block means the mover
/// image is old enough to have silently dropped the unknown `seed` field, fallen
/// into the create fallback, and initialized an EMPTY repository. Because this
/// block is mover-authored, its presence is proof the running image understood
/// the request — so every seed-armed success path must emit one, including the
/// no-op [`SeedOutcome::already_initialized`] case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedOutcome {
    /// Which copy mechanism the work spec selected. Set even when no copy ran.
    pub mode: crate::workspec::SeedModeSpec,
    /// The source rendering the controller pinned on the work spec, echoed back
    /// verbatim for `status.seed.source` (see
    /// [`crate::workspec::SeedOpSpec::source_description`] for why the mover
    /// does not re-derive it).
    pub source: String,
    /// `false` when the repository was ALREADY initialized and the seed was a
    /// documented no-op — the controller then reports `Seeded=True` with reason
    /// `AlreadyInitialized` rather than `Seeded`, and leaves `status.seed`
    /// counts unset.
    #[serde(default)]
    pub performed: bool,
    /// Snapshots observed at the SOURCE when the seed ran. `None` on the
    /// already-initialized no-op (nothing was opened). Zero is only ever
    /// recorded when `allowEmptySource` permitted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_count: Option<i64>,
    /// Snapshots that arrived at this repository — migrate mode only. A blob
    /// copy moves storage, not manifests, so it leaves this unset and the
    /// controller reports the post-seed catalog listing instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots_copied: Option<i64>,
}

impl SeedOutcome {
    /// The no-op outcome: a seed was armed, but the initial connect SUCCEEDED,
    /// so the repository was already initialized and nothing was copied.
    /// Emitted rather than omitted so the controller's mover-skew guard sees an
    /// acknowledgment — see the type docs.
    pub fn already_initialized(mode: crate::workspec::SeedModeSpec, source: String) -> Self {
        SeedOutcome {
            mode,
            source,
            performed: false,
            snapshot_count: None,
            snapshots_copied: None,
        }
    }

    /// The outcome of a seed that actually copied data.
    pub fn performed(
        mode: crate::workspec::SeedModeSpec,
        source: String,
        snapshot_count: i64,
        snapshots_copied: Option<i64>,
    ) -> Self {
        SeedOutcome {
            mode,
            source,
            performed: true,
            snapshot_count: Some(snapshot_count),
            snapshots_copied,
        }
    }
}

/// What the mover should do after its first bootstrap connect FAILED. Closed
/// enum + exhaustive `match`: a new outcome cannot compile until every caller
/// accounts for it, which for backup software is the difference between a
/// declined create and a silently emptied repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapInitAction {
    /// Do not initialize anything — report the failure. Covers a repository
    /// that exists but cannot be opened, a backend that denied or could not be
    /// reached, and the create/seed opt-outs.
    Fail,
    /// Initialize an EMPTY repository here (`kopia repository create`), the
    /// pre-#380 `spec.create.enabled` fallback.
    Create,
    /// Initialize this repository from `spec.seed`'s source.
    Seed,
}

/// Decide how a failed first bootstrap connect is answered (issue #380). Pure,
/// so the whole matrix is unit-tested without kopia.
///
/// `uninitialized` is the caller's `notfound_is_uninitialized(stderr)` verdict:
/// a `NotFound` is only "there is no repository here" when the backend ANSWERED
/// and the format blob is absent. A missing path or unbound mount also
/// classifies `NotFound` (`no such file or directory`), and treating that as an
/// empty backend is how a mis-mounted volume would get seeded — or created —
/// over.
///
/// The rules, in order:
///
/// 1. `AuthFailure` / `Locked` / `AccessDenied` / `PermissionDenied` ⇒ [`Fail`](BootstrapInitAction::Fail).
///    A repository exists here that we cannot open, another writer holds it, or
///    the backend refused us. Creating would risk a second repository and
///    seeding would write another cluster's data on top; both mask the real,
///    fixable error.
/// 2. Seed armed ⇒ [`Seed`](BootstrapInitAction::Seed), but ONLY on a genuinely
///    uninitialized `NotFound`. Anything else — an unreachable backend, an
///    unclassified error — is [`Fail`](BootstrapInitAction::Fail): a seed is a
///    one-shot whole-repository copy, and the create fallback is deliberately
///    NOT taken behind it (falling back would produce the empty repository
///    #380 is about).
/// 3. Otherwise the pre-#380 create gate: `auto_create` ⇒
///    [`Create`](BootstrapInitAction::Create), else
///    [`Fail`](BootstrapInitAction::Fail). kopia's own `create` refuses to
///    overwrite an existing repository, so this can never smash data.
///
/// [`should_attempt_create`] is exactly this function's `Create` arm, kept as
/// the narrow published predicate.
///
/// ```
/// use kopiur_kopia::KopiaErrorClass;
/// use kopiur_mover::bootstrap::{BootstrapInitAction, bootstrap_init_action};
///
/// // No seed, create enabled, repo genuinely absent ⇒ create an empty one.
/// assert_eq!(
///     bootstrap_init_action(false, true, KopiaErrorClass::NotFound, true),
///     BootstrapInitAction::Create
/// );
/// // Seed armed on the same miss ⇒ seed instead; create is never the fallback.
/// assert_eq!(
///     bootstrap_init_action(true, true, KopiaErrorClass::NotFound, true),
///     BootstrapInitAction::Seed
/// );
/// // A NotFound that is a missing mount, not an empty backend ⇒ never touch it.
/// assert_eq!(
///     bootstrap_init_action(true, true, KopiaErrorClass::NotFound, false),
///     BootstrapInitAction::Fail
/// );
/// // A repository we cannot open is never recreated and never seeded over.
/// assert_eq!(
///     bootstrap_init_action(true, true, KopiaErrorClass::AuthFailure, true),
///     BootstrapInitAction::Fail
/// );
/// ```
pub fn bootstrap_init_action(
    seed_armed: bool,
    auto_create: bool,
    class: KopiaErrorClass,
    uninitialized: bool,
) -> BootstrapInitAction {
    // Exhaustive, no `_ =>`: a new kopia error class must be classified here
    // before it can reach a repository-initializing decision.
    match class {
        KopiaErrorClass::AuthFailure
        | KopiaErrorClass::Locked
        | KopiaErrorClass::AccessDenied
        | KopiaErrorClass::PermissionDenied => BootstrapInitAction::Fail,
        KopiaErrorClass::NotFound => {
            if seed_armed {
                if uninitialized {
                    BootstrapInitAction::Seed
                } else {
                    BootstrapInitAction::Fail
                }
            } else if auto_create {
                BootstrapInitAction::Create
            } else {
                BootstrapInitAction::Fail
            }
        }
        KopiaErrorClass::RepositoryUnavailable
        | KopiaErrorClass::SourceError
        | KopiaErrorClass::Unknown => {
            // A seed never runs on an unclassified miss: the connect never got
            // far enough to prove the backend is empty, and a whole-repository
            // copy onto an unknown state is not a recoverable mistake.
            if seed_armed {
                BootstrapInitAction::Fail
            } else if auto_create {
                BootstrapInitAction::Create
            } else {
                BootstrapInitAction::Fail
            }
        }
    }
}

/// The outcome of a bootstrap run, serialized into the work-spec `ConfigMap`.
///
/// Constructed via [`BootstrapResult::ready`] (success) or
/// [`BootstrapResult::failed`] (a [`kopiur_kopia::KopiaError`]); it round-trips
/// through serde for the controller to read back:
///
/// ```
/// use kopiur_mover::bootstrap::BootstrapResult;
///
/// let r = BootstrapResult::ready(true, Some("deadbeef".into()), 3, vec![], false, 0, Some(7));
/// assert!(r.success && r.created);
/// assert_eq!(r.unique_id.as_deref(), Some("deadbeef"));
/// assert_eq!(r.snapshot_count, 3);
/// assert_eq!(r.index_blob_count, Some(7));
///
/// let json = serde_json::to_string(&r).unwrap();
/// let back: BootstrapResult = serde_json::from_str(&json).unwrap();
/// assert_eq!(back, r);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapResult {
    /// Whether connect/create succeeded and the repository is usable.
    pub success: bool,
    /// `true` when this run created a new repository (vs adopting an existing
    /// one). Drives the controller's "created" vs "connected" event.
    #[serde(default)]
    pub created: bool,
    /// The repository's stable kopia unique id (on success).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unique_id: Option<String>,
    /// Total snapshots in the repository (authoritative; not affected by the
    /// returned-entries cap OR the foreign-suffix prefilter).
    #[serde(default)]
    pub snapshot_count: i64,
    /// Snapshot entries for the controller to materialize as discovered Snapshots.
    /// Empty when `scanCatalog` was off, or capped to [`MAX_RETURNED_SNAPSHOTS`]
    /// (after [`apply_foreign_prefilter`] ran, when armed).
    #[serde(default)]
    pub snapshots: Vec<SnapshotListEntry>,
    /// `true` if more than [`MAX_RETURNED_SNAPSHOTS`] existed (after prefiltering)
    /// and the returned list was capped (so the controller can log that not all
    /// were materialized).
    #[serde(default)]
    pub snapshots_truncated: bool,
    /// Entries dropped by [`apply_foreign_prefilter`] BEFORE the
    /// [`MAX_RETURNED_SNAPSHOTS`] cap was applied (multi-cluster shared repo). `0`
    /// when the prefilter was off (`catalog_foreign_prefilter_cluster: None`). The
    /// controller's total foreign count is this value PLUS its own controller-side
    /// `ForeignIgnored`/foreign-classified decisions over the (already-filtered)
    /// returned entries — never double-counted, since a dropped entry never
    /// reaches the controller at all. Absent on old work-mover results (serde
    /// default).
    #[serde(default)]
    pub foreign_suffix_dropped: i64,
    /// Count of content-index blobs (`kopia index list`), when it could be read.
    /// Best-effort: `None` if the query failed (the controller then leaves the
    /// prior `status.storageStats.indexBlobCount` untouched). The controller
    /// warns when this crosses `spec.health.indexBlobWarnThreshold`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_blob_count: Option<i64>,
    /// The epoch parameters the repository reports (`repository status`), for the
    /// controller to mirror into `status.parameters.epoch` (#258). Best-effort: `None` when
    /// the status could not be read or the format predates epoch indexes. This is what
    /// makes the set-parameters apply honest — it is deliberately non-fatal, so a failed
    /// apply must remain VISIBLE as drift from `spec` rather than silently doing nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<kopiur_api::repository::ObservedEpochParameters>,
    /// Why `kopia repository set-parameters` did not apply, when it was asked to and
    /// failed (#258). `None` means "nothing to do, or it worked".
    ///
    /// The apply is deliberately best-effort — a bad parameter must not take an otherwise
    /// healthy repository to `Failed`, matching the maintenance-owner restamp a few lines
    /// above it. But best-effort must not mean SILENT: without this the only trace is a
    /// mover log line nobody reads and a `status.parameters.epoch` that quietly disagrees
    /// with `spec`. The controller turns this into a Warning event on the repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch_error: Option<String>,
    /// The blob retention the repository reports (`repository status`), for the controller
    /// to mirror into `status.parameters.blobRetention` (#332). Same best-effort contract as
    /// `epoch`: `None` when the status could not be read.
    ///
    /// There is deliberately no `blob_retention_error` sibling — epoch tuning and blob
    /// retention are applied by ONE `set-parameters` command, so they fail together and
    /// `epoch_error` carries the single reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_retention: Option<kopiur_api::repository::ObservedBlobRetention>,
    /// What `spec.seed` did on this run (issue #380), or `None` when the work
    /// spec armed no seed. On a seed-armed run this doubles as the controller's
    /// **mover-skew acknowledgment**: a success WITHOUT it means an older mover
    /// image dropped the unknown `seed` field and initialized an empty
    /// repository instead — see [`SeedOutcome`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<SeedOutcome>,
    /// Structured failure block on `success == false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureBlock>,
}

impl BootstrapResult {
    /// A successful bootstrap outcome.
    pub fn ready(
        created: bool,
        unique_id: Option<String>,
        snapshot_count: i64,
        snapshots: Vec<SnapshotListEntry>,
        snapshots_truncated: bool,
        foreign_suffix_dropped: i64,
        index_blob_count: Option<i64>,
    ) -> Self {
        BootstrapResult {
            success: true,
            created,
            unique_id,
            snapshot_count,
            snapshots,
            snapshots_truncated,
            foreign_suffix_dropped,
            index_blob_count,
            epoch: None,
            epoch_error: None,
            blob_retention: None,
            seed: None,
            failure: None,
        }
    }

    /// Attach the epoch parameters observed at the end of the bootstrap. Separate from
    /// [`BootstrapResult::ready`] rather than an eighth positional argument to it — the
    /// call site is already at the limit of what a positional list can carry legibly.
    pub fn with_epoch(
        mut self,
        epoch: Option<kopiur_api::repository::ObservedEpochParameters>,
        epoch_error: Option<String>,
    ) -> Self {
        self.epoch = epoch;
        self.epoch_error = epoch_error;
        self
    }

    /// Attach the blob retention observed at the end of the bootstrap (#332). A sibling of
    /// [`BootstrapResult::with_epoch`] rather than two more positional arguments to it, for
    /// the same reason that one exists.
    pub fn with_blob_retention(
        mut self,
        blob_retention: Option<kopiur_api::repository::ObservedBlobRetention>,
    ) -> Self {
        self.blob_retention = blob_retention;
        self
    }

    /// A terminal-failure outcome carrying the kopia error class + stderr tail.
    pub fn failed(err: &KopiaError) -> Self {
        BootstrapResult::with_failure(failure_block_from_kopia(err))
    }

    /// A terminal-failure outcome carrying a typed [`crate::error::MoverError`].
    ///
    /// The sibling of [`Self::failed`] for the bootstrap failures that are NOT
    /// kopia invocations — a seed whose source credentials could not be staged,
    /// or whose source password env is unset. Class, retry hint and message all
    /// come from the error itself (`From<&MoverError> for FailureBlock`), so
    /// they cannot drift from each other.
    pub fn from_mover_error(err: &crate::error::MoverError) -> Self {
        BootstrapResult::with_failure(FailureBlock::from(err))
    }

    /// The one place a FAILED [`BootstrapResult`] is built. Every failure
    /// constructor funnels through it so a new one cannot forget a field — and
    /// so the "a failed run reports no seed outcome" rule below has a single
    /// site rather than one copy per class.
    fn with_failure(failure: FailureBlock) -> Self {
        BootstrapResult {
            success: false,
            created: false,
            unique_id: None,
            snapshot_count: 0,
            snapshots: Vec::new(),
            snapshots_truncated: false,
            foreign_suffix_dropped: 0,
            index_blob_count: None,
            epoch: None,
            epoch_error: None,
            blob_retention: None,
            // A FAILED seed carries no outcome: the mover-skew guard only needs
            // an acknowledgment on the SUCCESS path (a failure already stops the
            // controller from reporting a Ready — and empty — repository), and a
            // half-populated outcome would read as a seed that partly worked.
            seed: None,
            failure: Some(failure),
        }
    }

    /// A terminal-failure outcome for "connect found no repository and
    /// `spec.create.enabled` is false". Unlike [`BootstrapResult::failed`] (which
    /// relays a kopia error class), this carries the
    /// [`REPOSITORY_NOT_INITIALIZED_CLASS`] sentinel + a fixed actionable message so
    /// the controller renders a dedicated `RepositoryNotInitialized` reason telling
    /// the operator to enable create — not a bare, confusing `NotFound`. Never
    /// retryable: it needs a spec change.
    pub fn not_initialized() -> Self {
        BootstrapResult::sentinel(
            REPOSITORY_NOT_INITIALIZED_CLASS,
            REPOSITORY_NOT_INITIALIZED_MESSAGE.to_string(),
            false,
        )
    }

    /// A terminal-failure outcome for a KOPIUR-decided (non-kopia) bootstrap
    /// verdict: a fixed sentinel `class` plus an actionable `message` the
    /// controller renders verbatim as a condition. `retry_recommended` says
    /// whether re-running the same pod could succeed without a spec change.
    ///
    /// One constructor for all of them so a new sentinel cannot forget a field
    /// (every seed failure class and [`Self::not_initialized`] go through it).
    fn sentinel(class: &str, message: String, retry_recommended: bool) -> Self {
        BootstrapResult::with_failure(FailureBlock {
            kopia_error_class: class.to_string(),
            message,
            stderr_tail: None,
            exit_code: None,
            retry_recommended,
            // A synthesized outcome, not a kopia invocation failure — the call
            // itself succeeded in reporting the state we refuse.
            op: None,
        })
    }

    /// A terminal-failure outcome for "`spec.seed`'s source backend answered but
    /// holds no kopia repository" ([`SEED_SOURCE_NOT_FOUND_CLASS`]). Retryable:
    /// the source may simply not exist YET (a replication that has not run), and
    /// the controller's recycle arm relaunches the bootstrap every ~2 minutes —
    /// which is the promptness disaster recovery wants.
    pub fn seed_source_not_found() -> Self {
        BootstrapResult::sentinel(
            SEED_SOURCE_NOT_FOUND_CLASS,
            SEED_SOURCE_NOT_FOUND_MESSAGE.to_string(),
            true,
        )
    }

    /// A terminal-failure outcome for "`spec.seed`'s source holds zero snapshots
    /// and `allowEmptySource` is false" ([`SEED_SOURCE_EMPTY_CLASS`]). Retryable
    /// for the same reason as [`Self::seed_source_not_found`]: a mirror that
    /// fills up later seeds on the next pass with no further action.
    pub fn seed_source_empty() -> Self {
        BootstrapResult::sentinel(
            SEED_SOURCE_EMPTY_CLASS,
            SEED_SOURCE_EMPTY_MESSAGE.to_string(),
            true,
        )
    }

    /// A terminal-failure outcome for a migrate-mode seed whose post-verify
    /// found snapshots missing at the destination ([`SEED_INCOMPLETE_CLASS`]).
    ///
    /// Unlike its two siblings the message interpolates counts and a bounded
    /// sample, because *which* snapshots did not arrive is the whole diagnostic
    /// — and unlike a per-attempt temp filename, those values are stable
    /// properties of the copy rather than of the pod.
    pub fn seed_incomplete(missing: usize, expected: usize, sample: &str) -> Self {
        BootstrapResult::sentinel(
            SEED_INCOMPLETE_CLASS,
            format!(
                "seeding is incomplete: {missing} of {expected} expected snapshot(s) did not \
                 arrive after `kopia snapshot migrate` (which exits 0 even when a per-source \
                 migration fails — see the bootstrap Job's pod logs for kopia's per-source \
                 errors). Missing (up to {cap} shown): {sample}. The bootstrap retries \
                 automatically and migrate is idempotent by (identity, startTime), so a retry \
                 copies only what is still missing; if it never converges, check the source \
                 repository's health with `kopia snapshot verify`",
                cap = crate::error::MISSING_SAMPLE_CAP
            ),
            true,
        )
    }

    /// Attach the [`SeedOutcome`] for a seed-armed run. Separate from
    /// [`BootstrapResult::ready`] for the same reason
    /// [`BootstrapResult::with_epoch`] is: the positional list is already at the
    /// limit of what stays legible.
    ///
    /// MUST be called on every seed-armed success — including the
    /// already-initialized no-op — or the controller treats the result as
    /// written by a mover too old to understand `spec.seed` (see [`SeedOutcome`]).
    pub fn with_seed(mut self, seed: Option<SeedOutcome>) -> Self {
        self.seed = seed;
        self
    }
}

/// Whether, after a failed connect, the mover should attempt `repository
/// create`. Pure so it is unit-tested without kopia.
///
/// Create is attempted only when `auto_create` is set AND the failure class does
/// not indicate an *existing* repository or a problem that `create` cannot fix:
/// - `AuthFailure` ⇒ a repo exists here that the password can't open — never
///   recreate (would risk a second repo / mask the real wrong-password error).
/// - `Locked` ⇒ a repo exists and is held by another writer — retry, don't create.
/// - `AccessDenied` ⇒ the backend denied access (bad creds, or the bucket/path
///   doesn't exist) — `create` would be denied too; don't mask it, surface the fix.
/// - `PermissionDenied` ⇒ the repo path isn't writable by our UID — `create`
///   would also fail with EACCES; surface the ownership/mode fix instead.
/// - everything else (`NotFound`, `RepositoryUnavailable`, `SourceError`,
///   `Unknown`) ⇒ attempt create. kopia's own `create` refuses to overwrite an
///   existing repository (the format blob backstop), so this can never smash
///   data; a genuinely unreachable backend simply fails `create` too, surfacing
///   the real error.
///
/// ```
/// use kopiur_kopia::KopiaErrorClass;
/// use kopiur_mover::bootstrap::should_attempt_create;
///
/// // Repo absent (or some other unclassified miss) + auto-create ⇒ create it.
/// assert!(should_attempt_create(true, KopiaErrorClass::NotFound));
/// // An existing repo we can't open (wrong password) must never be recreated.
/// assert!(!should_attempt_create(true, KopiaErrorClass::AuthFailure));
/// // auto-create off ⇒ never create, whatever the class.
/// assert!(!should_attempt_create(false, KopiaErrorClass::NotFound));
/// ```
pub fn should_attempt_create(auto_create: bool, class: KopiaErrorClass) -> bool {
    // Delegates rather than restating the gate: [`bootstrap_init_action`] is the
    // single decision, and this is its `Create` arm. `seed_armed: false` (no
    // seed is in play on this call), and `uninitialized` is then unread — it
    // only ever selects between Seed and Fail.
    matches!(
        bootstrap_init_action(false, auto_create, class, false),
        BootstrapInitAction::Create
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_blocked_on_auth_and_lock() {
        // An existing repo we can't open / is locked must never be recreated.
        assert!(!should_attempt_create(true, KopiaErrorClass::AuthFailure));
        assert!(!should_attempt_create(true, KopiaErrorClass::Locked));
    }

    #[test]
    fn create_attempted_for_absent_or_unknown_when_enabled() {
        assert!(should_attempt_create(true, KopiaErrorClass::NotFound));
        assert!(should_attempt_create(
            true,
            KopiaErrorClass::RepositoryUnavailable
        ));
        assert!(should_attempt_create(true, KopiaErrorClass::Unknown));
        assert!(should_attempt_create(true, KopiaErrorClass::SourceError));
    }

    #[test]
    fn create_never_attempted_when_disabled() {
        for class in [
            KopiaErrorClass::NotFound,
            KopiaErrorClass::AuthFailure,
            KopiaErrorClass::Unknown,
            KopiaErrorClass::RepositoryUnavailable,
        ] {
            assert!(!should_attempt_create(false, class));
        }
    }

    #[test]
    fn ready_result_roundtrips_via_serde() {
        let r = BootstrapResult::ready(true, Some("abc".into()), 3, vec![], false, 0, Some(42));
        let back: BootstrapResult =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert!(back.success && back.created);
        assert_eq!(back.unique_id.as_deref(), Some("abc"));
        assert_eq!(back.snapshot_count, 3);
        assert_eq!(back.foreign_suffix_dropped, 0);
        assert_eq!(back.index_blob_count, Some(42));
    }

    #[test]
    fn bootstrap_result_old_wire_json_without_foreign_suffix_dropped_still_decodes() {
        // Pre-M4 mover writes never carried this key.
        let old = r#"{"success":true,"created":false,"uniqueId":"abc","snapshotCount":3,
                       "snapshots":[],"snapshotsTruncated":false}"#;
        let parsed: BootstrapResult = serde_json::from_str(old).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.foreign_suffix_dropped, 0);
    }

    #[test]
    fn failed_result_roundtrips_with_failure_block() {
        let err = KopiaError::EmptyOutput {
            context: "repository status".into(),
            stderr_tail: String::new(),
        };
        let f = BootstrapResult::failed(&err);
        let back: BootstrapResult =
            serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(back, f);
        assert!(!back.success);
        assert_eq!(back.failure.unwrap().kopia_error_class, "Unknown");
    }

    #[test]
    fn not_initialized_is_an_actionable_non_retryable_failure() {
        let r = BootstrapResult::not_initialized();
        assert!(!r.success, "a not-initialized repo is a terminal failure");
        let f = r.failure.expect("not_initialized carries a failure block");
        // The sentinel class is what the controller keys on — it must be the shared
        // const, NOT a kopia class label, so it never collides with `from_label`.
        assert_eq!(f.kopia_error_class, REPOSITORY_NOT_INITIALIZED_CLASS);
        assert_ne!(
            f.kopia_error_class,
            KopiaErrorClass::NotFound.as_str(),
            "must not collapse back into a bare kopia NotFound"
        );
        // Actionable: the message names the exact field the operator must flip.
        assert!(
            f.message.contains("spec.create.enabled: true"),
            "message must tell the operator how to fix it, got: {}",
            f.message
        );
        // It needs a spec change, so the operator should not blindly retry.
        assert!(!f.retry_recommended);
    }

    #[test]
    fn not_initialized_roundtrips_via_serde() {
        let r = BootstrapResult::not_initialized();
        let back: BootstrapResult =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert_eq!(
            back.failure.unwrap().kopia_error_class,
            REPOSITORY_NOT_INITIALIZED_CLASS
        );
    }

    // --- apply_foreign_prefilter / prepare_catalog_entries ---

    fn entry(id: &str, host: &str) -> SnapshotListEntry {
        let now = chrono::Utc::now();
        SnapshotListEntry {
            id: id.into(),
            source: kopiur_kopia::SnapshotSource {
                user_name: "u".into(),
                host: host.into(),
                path: "/p".into(),
            },
            description: String::new(),
            start_time: now - chrono::Duration::seconds(60),
            end_time: now,
            stats: kopiur_kopia::SnapshotStats::default(),
            root_entry: None,
            retention_reason: vec![],
            tags: Default::default(),
        }
    }

    #[test]
    fn foreign_prefilter_drops_only_foreign_cluster_suffixed_entries() {
        let listing = vec![
            entry("a", "billing.east"),
            entry("b", "billing.west"),
            entry("c", "checkout"),
        ];
        let (kept, dropped) = apply_foreign_prefilter(listing, Some("east"));
        let kept_hosts: Vec<&str> = kept.iter().map(|e| e.source.host.as_str()).collect();
        assert_eq!(kept_hosts, vec!["billing.east", "checkout"]);
        assert_eq!(dropped, 1);
    }

    #[test]
    fn foreign_prefilter_none_drops_nothing() {
        // Would classify ForeignCluster if the prefilter were armed — but it isn't.
        let listing = vec![entry("a", "billing.west")];
        let (kept, dropped) = apply_foreign_prefilter(listing, None);
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn cap_applies_after_the_foreign_prefilter_so_dropped_entries_dont_count() {
        // 1000 own entries + 50 foreign ones = 1050 raw, which WOULD trip the cap if
        // it ran first — the prefilter drops the 50 foreign ones BEFORE the cap, so
        // all 1000 own entries come back untruncated.
        let mut listing: Vec<SnapshotListEntry> = (0..1000)
            .map(|i| entry(&format!("own-{i}"), "checkout"))
            .collect();
        listing.extend((0..50).map(|i| entry(&format!("foreign-{i}"), "billing.west")));
        let (kept, truncated, dropped) = prepare_catalog_entries(listing, Some("east"));
        assert_eq!(dropped, 50);
        assert!(
            !truncated,
            "the 1000 own entries fit under the cap once foreign ones are dropped"
        );
        assert_eq!(kept.len(), 1000);
    }

    #[test]
    fn cap_still_applies_when_the_prefilter_is_off() {
        let listing: Vec<SnapshotListEntry> = (0..(MAX_RETURNED_SNAPSHOTS + 10))
            .map(|i| entry(&format!("h{i}"), "checkout"))
            .collect();
        let (kept, truncated, dropped) = prepare_catalog_entries(listing, None);
        assert_eq!(dropped, 0);
        assert!(truncated);
        assert_eq!(kept.len(), MAX_RETURNED_SNAPSHOTS);
    }

    #[test]
    fn bare_hostname_foreign_entries_are_not_prefiltered_and_still_count_against_the_cap() {
        // A bare hostname (no `.`) is never classified ForeignCluster — the mover
        // cannot resolve it without a namespace lookup only the controller can do —
        // so it survives the prefilter and still occupies a cap slot.
        let listing = vec![entry("a", "ghost")];
        let (kept, dropped) = apply_foreign_prefilter(listing, Some("east"));
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 0);
    }

    // --- issue #237: ConfigMap size (slim entries + size guard) ---

    /// A realistic full-size list entry: a populated `rootEntry.summ` with an
    /// unbounded per-file `errors` list, a `retentionReason` array, and a
    /// `description` — the bloat that pushed real repositories past the 1 MiB
    /// ConfigMap limit at ~500 entries.
    fn fat_entry(id: &str) -> SnapshotListEntry {
        let now = chrono::Utc::now();
        let errors: Vec<kopiur_kopia::EntryError> = (0..20)
            .map(|i| kopiur_kopia::EntryError {
                path: format!("var/lib/app/data/subdir-{i}/some-long-filename-{i}.dat"),
                error: format!("error reading {i}: permission denied (os error 13)"),
            })
            .collect();
        SnapshotListEntry {
            id: id.into(),
            source: kopiur_kopia::SnapshotSource {
                user_name: "app-config".into(),
                host: "some-namespace".into(),
                path: "/pvc/app-config".into(),
            },
            description: "a fairly wordy free-form snapshot description that nobody reads".into(),
            start_time: now - chrono::Duration::seconds(60),
            end_time: now,
            stats: kopiur_kopia::SnapshotStats {
                total_size: 4096,
                ..Default::default()
            },
            root_entry: Some(kopiur_kopia::RootEntry {
                name: "app-config".into(),
                entry_type: "d".into(),
                obj: "kdeadbeefdeadbeefdeadbeef".into(),
                summary: Some(kopiur_kopia::DirSummary {
                    size: 4096,
                    files: 12,
                    symlinks: 0,
                    dirs: 3,
                    max_time: None,
                    num_failed: 20,
                    errors,
                }),
            }),
            retention_reason: vec!["latest-1".into(), "daily-1".into(), "weekly-1".into()],
            // Raw manifest tags: the reserved config tag, the kopiur-meta
            // payload, and an unbounded user tag — only the meta may survive.
            tags: BTreeMap::from([
                ("tag:kopiur".to_string(), "config:nightly".to_string()),
                (
                    "tag:kopiur-meta".to_string(),
                    r#"{"schema":1,"src":"explicit","uid":3001,"extra":true}"#.to_string(),
                ),
                ("tag:team".to_string(), "x".repeat(4096)),
            ]),
        }
    }

    #[test]
    fn slim_catalog_entry_drops_heavy_fields_keeps_consumed_ones() {
        let slim = slim_catalog_entry(fat_entry("k1"));
        // Dropped (never read by the controller).
        assert!(slim.root_entry.is_none());
        assert!(slim.retention_reason.is_empty());
        // Preserved (the catalog reads exactly these).
        assert_eq!(slim.id, "k1");
        assert_eq!(
            slim.source.identity(),
            "app-config@some-namespace:/pvc/app-config"
        );
        assert_eq!(slim.stats.total_size, 4096);
        // The description rides CAPPED (the catalog copies it onto the CR).
        assert_eq!(
            slim.description,
            "a fairly wordy free-form snapshot description that nobody reads"
        );
        // Tags are normalized: ONLY the canonical kopiur-meta payload survives —
        // raw user tags and the legacy config tag never ride the wire.
        assert_eq!(slim.tags.len(), 1, "{:?}", slim.tags);
        assert_eq!(
            slim.tags.get(KOPIUR_META_TAG).map(String::as_str),
            Some(r#"{"schema":1,"src":"explicit","uid":3001}"#),
            "canonical re-encode (unknown extras dropped, key bare)"
        );
    }

    #[test]
    fn slim_catalog_entry_caps_a_foreign_sized_description() {
        let mut e = fat_entry("k1");
        e.description = "é".repeat(DESCRIPTION_WIRE_CAP_BYTES); // 2 bytes/char
        let slim = slim_catalog_entry(e);
        assert!(slim.description.len() <= DESCRIPTION_WIRE_CAP_BYTES);
        assert!(slim.description.is_char_boundary(slim.description.len()));
    }

    #[test]
    fn normalize_meta_tags_keeps_undecodable_values_bounded_verbatim() {
        // A newer schema must reach the controller intact so it classifies
        // UnsupportedSchema (and counts it) — normalization must not eat it.
        let newer = BTreeMap::from([(
            "tag:kopiur-meta".to_string(),
            r#"{"schema":2,"src":"quantum","qbit":1}"#.to_string(),
        )]);
        let out = normalize_meta_tags(&newer);
        assert_eq!(
            out.get(KOPIUR_META_TAG).map(String::as_str),
            Some(r#"{"schema":2,"src":"quantum","qbit":1}"#)
        );
        assert!(matches!(
            decode_meta_tag(&out),
            MetaTagDecode::UnsupportedSchema { schema: 2 }
        ));

        // A forged multi-MB malformed value is truncated to the wire cap.
        let forged = BTreeMap::from([(
            "tag:kopiur-meta".to_string(),
            format!("{{{}", "x".repeat(1_000_000)),
        )]);
        let out = normalize_meta_tags(&forged);
        assert!(out.get(KOPIUR_META_TAG).unwrap().len() <= META_TAG_WIRE_CAP_BYTES);
        assert!(matches!(
            decode_meta_tag(&out),
            MetaTagDecode::Malformed { .. }
        ));

        // No meta at all → an EMPTY map (no `kopiur-meta` key minted from nothing).
        let none = BTreeMap::from([("tag:team".to_string(), "billing".to_string())]);
        assert!(normalize_meta_tags(&none).is_empty());
    }

    #[test]
    fn prepare_catalog_entries_returns_slimmed_entries() {
        let listing = vec![fat_entry("a"), fat_entry("b")];
        let (kept, truncated, _) = prepare_catalog_entries(listing, None);
        assert!(!truncated);
        assert!(
            kept.iter()
                .all(|e| e.root_entry.is_none() && e.retention_reason.is_empty()),
            "returned entries must be slimmed"
        );
    }

    #[test]
    fn full_catalog_of_fat_entries_fits_under_the_configmap_limit_once_slimmed() {
        const K8S_CONFIGMAP_LIMIT: usize = 1_048_576;
        let raw: Vec<SnapshotListEntry> = (0..MAX_RETURNED_SNAPSHOTS)
            .map(|i| fat_entry(&format!("k{i}")))
            .collect();

        // Without slimming, a full cap's worth of real entries blows past 1 MiB —
        // this is the bug: the count cap is not a size cap.
        let unslimmed = BootstrapResult::ready(
            false,
            Some("u".into()),
            raw.len() as i64,
            raw.clone(),
            false,
            0,
            Some(1),
        );
        assert!(
            serde_json::to_string(&unslimmed).unwrap().len() > K8S_CONFIGMAP_LIMIT,
            "the test fixture must actually exceed the limit unslimmed, else it proves nothing"
        );

        // Slimmed via the real prepare path, the same 1000 entries fit comfortably.
        let (slim, _, _) = prepare_catalog_entries(raw, None);
        let result = BootstrapResult::ready(
            false,
            Some("u".into()),
            slim.len() as i64,
            slim,
            false,
            0,
            Some(1),
        );
        let size = serde_json::to_string(&result).unwrap().len();
        assert!(
            size < RESULT_SIZE_BUDGET_BYTES,
            "slimmed 1000-entry result must fit the budget, was {size} bytes"
        );
    }

    #[test]
    fn enforce_result_size_budget_trims_over_budget_and_flags_truncated() {
        let slim: Vec<SnapshotListEntry> = (0..200)
            .map(|i| slim_catalog_entry(fat_entry(&format!("k{i}"))))
            .collect();
        let result = BootstrapResult::ready(false, Some("u".into()), 200, slim, false, 0, Some(1));
        // A deliberately tiny budget forces trimming.
        let guarded = enforce_result_size_budget(result, 4_096);
        assert!(guarded.snapshots.len() < 200, "must have dropped entries");
        assert!(guarded.snapshots_truncated, "must flag the truncation");
        // The authoritative count is never rewritten by the size guard.
        assert_eq!(guarded.snapshot_count, 200);
        assert!(
            serde_json::to_string(&guarded).unwrap().len() <= 4_096 || guarded.snapshots.is_empty(),
            "trims until it fits (or nothing is left to drop)"
        );
    }

    #[test]
    fn enforce_result_size_budget_is_a_noop_under_budget() {
        let slim = vec![slim_catalog_entry(fat_entry("k1"))];
        let result = BootstrapResult::ready(false, Some("u".into()), 1, slim, false, 0, Some(1));
        let before = result.clone();
        let guarded = enforce_result_size_budget(result, RESULT_SIZE_BUDGET_BYTES);
        assert_eq!(
            guarded, before,
            "an in-budget result must be left untouched"
        );
    }
}
