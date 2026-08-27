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

/// Sentinel [`FailureBlock::kopia_error_class`] for the case a `spec.seed` was
/// armed but the repository ends the bootstrap holding ZERO snapshots, with
/// `allowEmptySource` at its `false` default (issue #380).
///
/// This is the backstop for the one path the source-side gates cannot see. A
/// seed that dies AFTER initializing the backend — a migrate whose
/// `repository create` succeeded before the copy failed, a `sync-to` killed by
/// the Job deadline mid-copy, an OOM — leaves an initialized but empty
/// repository behind. The NEXT bootstrap's connect then SUCCEEDS, the seed
/// reports itself a documented no-op ("already initialized"), and the
/// repository would go `Ready` with no history: exactly the failure #380
/// exists to prevent, re-entered through the back door.
///
/// **Retryable under the same resume contract as [`SEED_INCOMPLETE_CLASS`]**,
/// and for the same reason: with `resume` the next bootstrap re-runs the copy
/// against the already-initialized backend instead of reporting the
/// `AlreadyInitialized` no-op, so an interrupted seed converges. Without
/// `resume` a retry would find the same initialized-and-empty repository
/// forever — so a controller that never sets it must treat this as terminal
/// rather than spin on it.
///
/// The class is kept even though `resume` makes the underlying state
/// recoverable: it is the assertion that stops a seed-armed bootstrap from
/// reporting SUCCESS over an empty repository. Refusing is what turns a silent
/// wrong answer into a visible, converging one.
pub const SEED_LEFT_EMPTY_CLASS: &str = "SeedLeftEmpty";

/// Stable, volatile-free actionable message for [`SEED_LEFT_EMPTY_CLASS`].
pub const SEED_LEFT_EMPTY_MESSAGE: &str = "spec.seed is set and this repository is initialized but holds ZERO snapshots, so reporting \
     it Ready would hand you a repository with no history. This normally means an earlier seed \
     attempt initialized the backend and then failed (a mover killed by the Job deadline, an \
     OOM, or a copy that errored after `repository create`). Kopiur retries the seed itself — it \
     records that an attempt started and resumes the copy on the next bootstrap, so no manual \
     cleanup is needed and nothing at the backend should be deleted. If it keeps recurring, read \
     the bootstrap Job's pod logs for why each attempt stops (a deadline too short for the \
     repository's size is the usual cause — raise \
     spec.seed.failurePolicy.activeDeadlineSeconds). If this repository really is meant to start \
     out empty, set spec.seed.allowEmptySource: true";

/// Whether a bootstrap must refuse to report success because a seed was armed
/// yet the repository ends up empty. See [`SEED_LEFT_EMPTY_CLASS`] for the
/// failure this catches. Pure.
///
/// Deliberately keyed on the OUTCOME (the repository holds nothing) rather than
/// on which seed path ran: the same refusal is correct whether the seed copied
/// nothing, was a no-op over a repository a previous attempt left behind, or
/// something else emptied it. A seed that legitimately copied an empty source
/// can only have got here with `allow_empty_source` set, which switches the
/// guard off.
pub fn seed_left_repository_empty(
    seed_armed: bool,
    allow_empty_source: bool,
    snapshot_count: i64,
) -> bool {
    seed_armed && !allow_empty_source && snapshot_count == 0
}

/// Sentinel [`FailureBlock::kopia_error_class`] for a migrate-mode seed where
/// `kopia snapshot migrate` exited 0 but the post-verify found snapshots
/// missing at the destination (issue #380).
///
/// Mandatory because kopia's per-source migration goroutines only LOG their
/// errors — a zero exit does NOT mean every snapshot arrived, so the
/// destination listing is the only honest success signal.
///
/// **Retryable, but ONLY under the resume contract.** `snapshot migrate` is
/// idempotent by `(identity, startTime)`, so a retry copies just what is still
/// missing — but a retry only *reaches* the copy if the next bootstrap runs the
/// seed again. By the time this fires the local repository EXISTS, so the next
/// connect succeeds and, without [`crate::workspec::SeedOpSpec::resume`], the
/// seed would report the `AlreadyInitialized` no-op and the repository would go
/// `Ready` carrying exactly the incomplete history this failure just refused.
///
/// The contract, therefore: **a controller that never sets `resume` must not
/// treat this class as retryable.** The controller stamps a durable
/// seed-attempt marker before creating a seeding Job and passes `resume: true`
/// while that marker exists and `status.seed.seededAt` does not (issue #380
/// stage C3). The marker is also the second lock — it is what keeps an
/// interrupted seed from being mistaken for an ordinary adoption.
pub const SEED_INCOMPLETE_CLASS: &str = "SeedIncomplete";

/// Sentinel [`FailureBlock::kopia_error_class`] for a state the mover believes
/// is unreachable having reached it anyway — a broken internal invariant, not
/// anything about the repository or its backend.
///
/// Deliberately **terminal**. Every other bootstrap failure class describes a
/// world that can change (a source that will exist later, a copy that will
/// finish), so retrying them is progress. This one describes the mover
/// disagreeing with itself: the same inputs produce the same contradiction on
/// every attempt, so a retryable classification would spin a Job every two
/// minutes forever while hiding the bug behind it. Failing terminally puts the
/// message somewhere a human reads.
pub const BOOTSTRAP_INTERNAL_INCONSISTENCY_CLASS: &str = "BootstrapInternalInconsistency";

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
    /// Snapshots PRESENT at this repository after the copy — migrate mode only.
    /// A blob copy moves storage, not manifests, so it leaves this unset and the
    /// controller reports the post-seed catalog listing instead.
    ///
    /// **Cumulative, not per-run.** On a first seed the repository was empty
    /// beforehand, so "present now" and "copied by this run" are the same
    /// number. On a RESUME
    /// ([`crate::workspec::SeedOpSpec::resume`]) it includes what the
    /// interrupted attempt had already moved. Chosen deliberately: the question
    /// `status.seed.snapshotsCopied` should answer is "how much of the source is
    /// here now", which stays meaningful across however many attempts a recovery
    /// took — a per-run delta would report a small number on the very run that
    /// finished the job.
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

/// What the mover should do about initializing the repository, after its first
/// bootstrap connect. Closed enum + exhaustive `match`: a new outcome cannot
/// compile until every caller accounts for it, which for backup software is the
/// difference between a declined create and a silently emptied repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapInitAction {
    /// The repository is connected and usable as-is — initialize nothing and
    /// carry on with the rest of the bootstrap. The ordinary connect-to-existing
    /// outcome, and the one a standing (non-resuming) `spec.seed` gets on an
    /// already-initialized repository.
    Proceed,
    /// Do not initialize anything — report the failure. Covers a repository
    /// that exists but cannot be opened, a backend that denied or could not be
    /// reached, and the create/seed opt-outs.
    Fail,
    /// Initialize an EMPTY repository here (`kopia repository create`), the
    /// pre-#380 `spec.create.enabled` fallback.
    Create,
    /// Run `spec.seed`: either initializing this repository from its source, or
    /// — when `resume` is set and the backend is already initialized —
    /// finishing a copy a previous attempt started.
    Seed,
}

/// Decide how the first bootstrap connect is answered (issue #380). Pure, so
/// the whole matrix is unit-tested without kopia.
///
/// `connect_class` is `None` when the connect SUCCEEDED and `Some(class)` when
/// it failed. `uninitialized` is the caller's
/// `notfound_is_uninitialized(stderr)` verdict: a `NotFound` is only "there is
/// no repository here" when the backend ANSWERED and the format blob is absent.
/// A missing path or unbound mount also classifies `NotFound` (`no such file or
/// directory`), and treating that as an empty backend is how a mis-mounted
/// volume would get seeded — or created — over.
///
/// The rules, in order:
///
/// 1. **The connect succeeded.** The repository exists and opens.
///    * `resume` (with a seed armed) ⇒ [`Seed`](BootstrapInitAction::Seed): a
///      previous attempt started a copy and did not finish it, so re-run it
///      against the existing backend. Without this edge an interrupted seed is
///      unrecoverable by retry — the retry connects fine, reports the
///      `AlreadyInitialized` no-op, and the repository goes `Ready` with
///      partial history.
///    * otherwise ⇒ [`Proceed`](BootstrapInitAction::Proceed). A standing
///      `spec.seed` over a repository someone else initialized is the
///      documented no-op; clobbering it would be the opposite of safe.
/// 2. `AuthFailure` / `Locked` / `AccessDenied` / `PermissionDenied` ⇒
///    [`Fail`](BootstrapInitAction::Fail), whatever else is set. A repository
///    exists here that we cannot open, another writer holds it, or the backend
///    refused us. Creating would risk a second repository and seeding would
///    write another cluster's data over a state we could not even read; both
///    mask the real, fixable error.
/// 3. Seed armed ⇒ [`Seed`](BootstrapInitAction::Seed), but ONLY on a genuinely
///    uninitialized `NotFound`. Anything else — an unreachable backend, an
///    unclassified error — is [`Fail`](BootstrapInitAction::Fail): a seed is a
///    whole-repository copy, and the create fallback is deliberately NOT taken
///    behind it (falling back would produce the empty repository #380 is about).
/// 4. Otherwise the pre-#380 create gate: `auto_create` ⇒
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
/// // Connect succeeded, nothing to do.
/// assert_eq!(
///     bootstrap_init_action(false, false, true, None, false),
///     BootstrapInitAction::Proceed
/// );
/// // Connect succeeded, a seed is armed but not resuming: the documented no-op.
/// assert_eq!(
///     bootstrap_init_action(true, false, false, None, false),
///     BootstrapInitAction::Proceed
/// );
/// // Connect succeeded and we are RESUMING an interrupted seed: run it anyway.
/// assert_eq!(
///     bootstrap_init_action(true, true, false, None, false),
///     BootstrapInitAction::Seed
/// );
/// // No seed, create enabled, repo genuinely absent ⇒ create an empty one.
/// assert_eq!(
///     bootstrap_init_action(false, false, true, Some(KopiaErrorClass::NotFound), true),
///     BootstrapInitAction::Create
/// );
/// // Seed armed on the same miss ⇒ seed instead; create is never the fallback.
/// assert_eq!(
///     bootstrap_init_action(true, false, true, Some(KopiaErrorClass::NotFound), true),
///     BootstrapInitAction::Seed
/// );
/// // A NotFound that is a missing mount, not an empty backend ⇒ never touch it.
/// assert_eq!(
///     bootstrap_init_action(true, false, true, Some(KopiaErrorClass::NotFound), false),
///     BootstrapInitAction::Fail
/// );
/// // A repository we cannot open is never recreated and never seeded over.
/// assert_eq!(
///     bootstrap_init_action(true, true, true, Some(KopiaErrorClass::AuthFailure), true),
///     BootstrapInitAction::Fail
/// );
/// ```
pub fn bootstrap_init_action(
    seed_armed: bool,
    resume: bool,
    auto_create: bool,
    connect_class: Option<KopiaErrorClass>,
    uninitialized: bool,
) -> BootstrapInitAction {
    let Some(class) = connect_class else {
        // The connect succeeded: the repository exists and opens. Only a
        // RESUMING seed has work to do against it.
        return if seed_armed && resume {
            BootstrapInitAction::Seed
        } else {
            BootstrapInitAction::Proceed
        };
    };
    // Exhaustive, no `_ =>`: a new kopia error class must be classified here
    // before it can reach a repository-initializing decision.
    match class {
        KopiaErrorClass::AuthFailure
        | KopiaErrorClass::Locked
        | KopiaErrorClass::AccessDenied
        | KopiaErrorClass::PermissionDenied => BootstrapInitAction::Fail,
        KopiaErrorClass::NotFound => {
            if seed_armed {
                // `resume` does not gate this arm: a marker-bearing repository
                // whose backend turns out to be genuinely empty (the copy died
                // before writing the format blob, or someone cleared it) still
                // needs the seed run from the start.
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
            // far enough to prove anything about the backend, and a
            // whole-repository copy onto an unknown state is not a recoverable
            // mistake.
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

/// The [`SeedOutcome`] a bootstrap whose connect SUCCEEDED must report, or
/// `None` when there is none to report.
///
/// Pure, and the testable core of the mover-skew invariant: **every seed-armed
/// SUCCESS carries a `seed` block**, the standing no-op included, because its
/// presence is what proves to the controller that this mover image understood
/// `spec.seed` at all.
///
/// * no seed armed ⇒ `None` (nothing to acknowledge).
/// * seed armed, NOT resuming ⇒ the `AlreadyInitialized` no-op: the repository
///   was initialized by someone else and must not be clobbered.
/// * seed armed and RESUMING ⇒ `None` **here**, because the seed is about to
///   actually run ([`BootstrapInitAction::Seed`]) and will report what it did.
///   Emitting the no-op alongside a real run is the one way this could report a
///   seed that never happened.
pub fn already_initialized_outcome(
    seed: Option<&crate::workspec::SeedOpSpec>,
) -> Option<SeedOutcome> {
    let seed = seed?;
    if seed.resume {
        return None;
    }
    Some(SeedOutcome::already_initialized(
        seed.mode(),
        seed.source_description.clone(),
    ))
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
/// let r =
///     BootstrapResult::ready(true, Some("deadbeef".into()), Some(3), vec![], false, 0, Some(7));
/// assert!(r.success && r.created);
/// assert_eq!(r.unique_id.as_deref(), Some("deadbeef"));
/// assert_eq!(r.snapshot_count, Some(3));
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
    /// returned-entries cap OR the foreign-suffix prefilter). `None` means the
    /// listing DID NOT RUN this run (a `probe_only` bootstrap, #414 — the
    /// listing is the O(snapshots) step a health probe doesn't need): the
    /// controller must then leave `storageStats.snapshotCount` and the
    /// discovered-Snapshot catalog untouched. Never `None` on a run that
    /// listed — old movers always wrote the field, and only a new controller
    /// ever arms `probe_only`, so version skew cannot produce a false `None`
    /// on a listing run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_count: Option<i64>,
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
    /// A successful bootstrap outcome. `snapshot_count: None` = the catalog
    /// listing did not run (a `probe_only` bootstrap).
    pub fn ready(
        created: bool,
        unique_id: Option<String>,
        snapshot_count: Option<i64>,
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
            snapshot_count: None,
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

    /// A terminal-failure outcome for a broken internal invariant
    /// ([`BOOTSTRAP_INTERNAL_INCONSISTENCY_CLASS`]). `detail` names the
    /// contradiction; it is a fixed, per-call-site string, never a per-attempt
    /// value, because the controller renders the message verbatim as a
    /// condition.
    pub fn internal_inconsistency(detail: &str) -> Self {
        BootstrapResult::sentinel(
            BOOTSTRAP_INTERNAL_INCONSISTENCY_CLASS,
            format!(
                "the bootstrap mover reached a state it believes is impossible: {detail}. This \
                 is a defect in kopiur, not a problem with this repository or its backend, so \
                 retrying will reproduce it — the failure is terminal on purpose. Please report \
                 it with the bootstrap Job's pod logs and the repository's spec"
            ),
            false,
        )
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

    /// A terminal-failure outcome for "a seed was armed but this repository is
    /// initialized and empty" ([`SEED_LEFT_EMPTY_CLASS`]). Retryable **under the
    /// resume contract** — see that constant.
    pub fn seed_left_empty() -> Self {
        BootstrapResult::sentinel(
            SEED_LEFT_EMPTY_CLASS,
            SEED_LEFT_EMPTY_MESSAGE.to_string(),
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
                 automatically and migrate is idempotent by (identity, startTime), so it \
                 resumes the copy and moves only what is still missing; if it never converges, \
                 check the source repository's health with `kopia snapshot verify` and the \
                 bootstrap Job's pod logs for kopia's per-source errors",
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
    // seed is in play on this call, so `resume` is moot too), and
    // `uninitialized` is then unread — it only ever selects between Seed and
    // Fail. `Some(class)` because a create decision only arises from a FAILED
    // connect.
    matches!(
        bootstrap_init_action(false, false, auto_create, Some(class), false),
        BootstrapInitAction::Create
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- #380: the seed init decision + seed outcomes ----------------------

    /// Every `KopiaErrorClass`, so the matrices below cannot silently skip one
    /// a future variant adds.
    const ALL_CLASSES: [KopiaErrorClass; 8] = [
        KopiaErrorClass::RepositoryUnavailable,
        KopiaErrorClass::AuthFailure,
        KopiaErrorClass::AccessDenied,
        KopiaErrorClass::PermissionDenied,
        KopiaErrorClass::NotFound,
        KopiaErrorClass::Locked,
        KopiaErrorClass::SourceError,
        KopiaErrorClass::Unknown,
    ];

    /// A blob seed fixture whose `resume` flag the caller chooses.
    fn sample_seed(resume: bool) -> crate::workspec::SeedOpSpec {
        crate::workspec::SeedOpSpec {
            from: crate::workspec::SeedConnectSource::Backend(Box::new(
                crate::workspec::RepositoryConnect::Filesystem {
                    path: "/mnt/mirror".into(),
                },
            )),
            source_description: "S3".into(),
            sync: None,
            migrate: None,
            allow_empty_source: false,
            resume,
        }
    }

    #[test]
    fn an_unopenable_repository_is_never_created_and_never_seeded_over() {
        // A repo exists here we can't open (wrong password), another writer
        // holds it, or the backend refused us. Creating risks a second
        // repository; seeding would write another cluster's data over a state
        // we could not even read. Both are unrecoverable mistakes, so neither
        // is ever attempted — for ANY combination of the two opt-ins.
        for class in [
            KopiaErrorClass::AuthFailure,
            KopiaErrorClass::Locked,
            KopiaErrorClass::AccessDenied,
            KopiaErrorClass::PermissionDenied,
        ] {
            for seed in [false, true] {
                for create in [false, true] {
                    for uninit in [false, true] {
                        for resume in [false, true] {
                            assert_eq!(
                                bootstrap_init_action(seed, resume, create, Some(class), uninit),
                                BootstrapInitAction::Fail,
                                "{class:?} seed={seed} resume={resume} create={create} uninit={uninit}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn an_armed_seed_runs_only_on_a_genuinely_uninitialized_backend() {
        // The `uninitialized` half is what keeps a mis-mounted volume from
        // being seeded over: a missing path classifies `NotFound` too ("no such
        // file or directory"), and only kopia's "repository not initialized"
        // proves the backend answered and is simply empty.
        assert_eq!(
            bootstrap_init_action(true, false, false, Some(KopiaErrorClass::NotFound), true),
            BootstrapInitAction::Seed
        );
        assert_eq!(
            bootstrap_init_action(true, false, false, Some(KopiaErrorClass::NotFound), false),
            BootstrapInitAction::Fail
        );
        // `auto_create` does not gate a seed either way — a seed is the
        // initialization the user explicitly asked for, not the create fallback.
        assert_eq!(
            bootstrap_init_action(true, false, true, Some(KopiaErrorClass::NotFound), true),
            BootstrapInitAction::Seed
        );
        // A marker-bearing (resuming) seed over a backend that turns out to be
        // genuinely EMPTY still seeds from the start — the previous attempt died
        // before writing a format blob, or someone cleared it.
        assert_eq!(
            bootstrap_init_action(true, true, false, Some(KopiaErrorClass::NotFound), true),
            BootstrapInitAction::Seed
        );
    }

    #[test]
    fn an_armed_seed_never_falls_back_to_creating_an_empty_repository() {
        // THE #380 invariant. Any class that is not a proven-empty backend must
        // Fail rather than Create: falling back would report a `Ready` but
        // EMPTY repository, which is the data-loss shape the feature exists to
        // prevent. Note `auto_create: true` throughout — the fallback is armed
        // and still must not fire — and both `resume` states, since neither may
        // reopen the create path.
        for class in ALL_CLASSES {
            for resume in [false, true] {
                assert_eq!(
                    bootstrap_init_action(true, resume, true, Some(class), false),
                    BootstrapInitAction::Fail,
                    "no proof the backend is empty ⇒ neither create nor seed ({class:?}, resume={resume})"
                );
            }
        }
        // WITH the "backend is empty" proof, the outcome is exactly Seed for a
        // genuine NotFound and Fail for everything else — never Create.
        for class in ALL_CLASSES {
            for resume in [false, true] {
                let expected = if class == KopiaErrorClass::NotFound {
                    BootstrapInitAction::Seed
                } else {
                    BootstrapInitAction::Fail
                };
                assert_eq!(
                    bootstrap_init_action(true, resume, true, Some(class), true),
                    expected,
                    "{class:?} resume={resume}"
                );
            }
        }
    }

    #[test]
    fn a_resuming_seed_re_runs_over_an_already_initialized_repository() {
        // The connect SUCCEEDED — the repository exists and opens. Without
        // `resume` that is the documented standing no-op; WITH it, a previous
        // attempt left a copy unfinished and the seed must actually run, or the
        // retry Readies partial history (issue #380).
        assert_eq!(
            bootstrap_init_action(true, true, false, None, false),
            BootstrapInitAction::Seed
        );
        assert_eq!(
            bootstrap_init_action(true, false, false, None, false),
            BootstrapInitAction::Proceed
        );
        // `resume` is meaningless without a seed armed, and must never invent
        // work on a repository that simply connected.
        for resume in [false, true] {
            for auto_create in [false, true] {
                assert_eq!(
                    bootstrap_init_action(false, resume, auto_create, None, false),
                    BootstrapInitAction::Proceed,
                    "resume={resume} auto_create={auto_create}"
                );
            }
        }
    }

    #[test]
    fn resume_never_reaches_create_and_never_survives_the_denylist() {
        // Two invariants the resume edge must not weaken:
        //  * it is a SEED lever, never a create one;
        //  * a repository we cannot open is still never touched, marker or not.
        for class in ALL_CLASSES {
            for uninit in [false, true] {
                assert_ne!(
                    bootstrap_init_action(true, true, true, Some(class), uninit),
                    BootstrapInitAction::Create,
                    "{class:?} uninit={uninit}"
                );
            }
        }
        for class in [
            KopiaErrorClass::AuthFailure,
            KopiaErrorClass::Locked,
            KopiaErrorClass::AccessDenied,
            KopiaErrorClass::PermissionDenied,
        ] {
            assert_eq!(
                bootstrap_init_action(true, true, true, Some(class), true),
                BootstrapInitAction::Fail,
                "{class:?}"
            );
        }
    }

    #[test]
    fn the_already_initialized_outcome_is_emitted_only_when_not_resuming() {
        // The testable core of the mover-skew invariant: every seed-armed
        // SUCCESS carries a `seed` block, so its presence proves this mover
        // image understood `spec.seed`.
        let seed = sample_seed(false);
        let outcome = already_initialized_outcome(Some(&seed)).expect("a no-op outcome");
        assert!(!outcome.performed);
        assert_eq!(outcome.mode, crate::workspec::SeedModeSpec::Blob);
        assert_eq!(outcome.source, "S3");
        assert!(outcome.snapshot_count.is_none());

        // RESUMING: none here, because the seed is about to actually run and
        // will report what it did. Emitting the no-op alongside a real run is
        // the one way this could report a seed that never happened.
        assert!(already_initialized_outcome(Some(&sample_seed(true))).is_none());

        // No seed armed ⇒ nothing to acknowledge.
        assert!(already_initialized_outcome(None).is_none());
    }

    #[test]
    fn without_a_seed_the_decision_is_exactly_the_old_create_gate() {
        // `should_attempt_create` is now this function's Create arm; the two
        // must agree for every class and both opt-in states, so the #380
        // refactor cannot have changed a single pre-existing decision.
        for class in ALL_CLASSES {
            for create in [false, true] {
                for uninit in [false, true] {
                    let creates = bootstrap_init_action(false, false, create, Some(class), uninit)
                        == BootstrapInitAction::Create;
                    assert_eq!(
                        creates,
                        should_attempt_create(create, class),
                        "{class:?} create={create} uninit={uninit}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_seed_outcome_round_trips_and_distinguishes_the_no_op() {
        let performed = SeedOutcome::performed(
            crate::workspec::SeedModeSpec::Migrate,
            "ClusterRepository/offsite".into(),
            42,
            Some(42),
        );
        assert!(performed.performed);
        let v = serde_json::to_value(&performed).unwrap();
        assert_eq!(v["mode"], "migrate");
        assert_eq!(v["source"], "ClusterRepository/offsite");
        assert_eq!(v["snapshotCount"], 42);
        assert_eq!(v["snapshotsCopied"], 42);
        let back: SeedOutcome = serde_json::from_value(v).unwrap();
        assert_eq!(back, performed);

        // The already-initialized no-op still names mode + source (the
        // controller renders `status.seed` from them), but carries no counts:
        // nothing was opened, so reporting zero would be a lie.
        let noop =
            SeedOutcome::already_initialized(crate::workspec::SeedModeSpec::Blob, "S3".into());
        assert!(!noop.performed);
        assert!(noop.snapshot_count.is_none());
        assert!(noop.snapshots_copied.is_none());
        let v = serde_json::to_value(&noop).unwrap();
        assert!(v.get("snapshotCount").is_none());
        assert!(v.get("snapshotsCopied").is_none());
        assert_eq!(serde_json::from_value::<SeedOutcome>(v).unwrap(), noop);

        // A blob copy moves storage, not manifests — no per-snapshot count.
        let blob =
            SeedOutcome::performed(crate::workspec::SeedModeSpec::Blob, "S3".into(), 7, None);
        assert_eq!(blob.snapshot_count, Some(7));
        assert!(blob.snapshots_copied.is_none());
    }

    #[test]
    fn a_bootstrap_result_carries_its_seed_outcome_and_still_decodes_old_wire() {
        let r = BootstrapResult::ready(false, Some("abc".into()), Some(3), vec![], false, 0, None)
            .with_seed(Some(SeedOutcome::performed(
                crate::workspec::SeedModeSpec::Blob,
                "S3".into(),
                3,
                None,
            )));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["seed"]["mode"], "blob");
        assert_eq!(v["seed"]["performed"], true);
        // A blob seed leaves `created` false — only the create fallback creates.
        assert_eq!(v["created"], false);
        assert_eq!(serde_json::from_value::<BootstrapResult>(v).unwrap(), r);

        // Old wire (and every unseeded bootstrap): no `seed` key at all, which
        // must decode to `None` and re-serialize without one.
        let old = r#"{"success":true,"created":true,"uniqueId":"abc","snapshotCount":0,
                       "snapshots":[],"snapshotsTruncated":false}"#;
        let parsed: BootstrapResult = serde_json::from_str(old).unwrap();
        assert!(parsed.seed.is_none());
        assert!(serde_json::to_value(&parsed).unwrap().get("seed").is_none());
    }

    #[test]
    fn seed_failures_are_retryable_and_carry_no_partial_outcome() {
        // D9 plus the two the resume contract added: a dead or empty mirror, an
        // incomplete migrate, and a repository an interrupted seed left empty
        // all route to the controller's recycle arm and retry every couple of
        // minutes — the promptness disaster recovery wants — rather than needing
        // a spec edit like `not_initialized` does. The last two are retryable
        // ONLY because the controller's marker + `resume` make the retry
        // actually re-run the copy; see their class docs.
        for (result, class) in [
            (
                BootstrapResult::seed_source_not_found(),
                SEED_SOURCE_NOT_FOUND_CLASS,
            ),
            (
                BootstrapResult::seed_source_empty(),
                SEED_SOURCE_EMPTY_CLASS,
            ),
            (
                BootstrapResult::seed_incomplete(2, 5, "a@b:/c@2026-01-01T00:00:00Z"),
                SEED_INCOMPLETE_CLASS,
            ),
            (BootstrapResult::seed_left_empty(), SEED_LEFT_EMPTY_CLASS),
        ] {
            assert!(!result.success, "{class}");
            // A failure never reports a seed outcome: a half-populated one would
            // read as a seed that partly worked.
            assert!(result.seed.is_none(), "{class}");
            let failure = result.failure.expect("failure block");
            assert_eq!(failure.kopia_error_class, class);
            assert!(failure.retry_recommended, "{class} must be retryable");
            assert!(!failure.message.is_empty(), "{class}");
        }
        // The pre-existing sentinel is unchanged: it needs a spec change, so it
        // is deliberately NOT retryable.
        let r = BootstrapResult::not_initialized();
        let f = r.failure.expect("failure block");
        assert_eq!(f.kopia_error_class, REPOSITORY_NOT_INITIALIZED_CLASS);
        assert!(!f.retry_recommended);
        assert_eq!(f.message, REPOSITORY_NOT_INITIALIZED_MESSAGE);
    }

    #[test]
    fn a_seed_that_left_the_repository_empty_is_refused() {
        // The backstop for the one path the source-side gates cannot see: an
        // earlier seed initialized the backend and then died, so the retry's
        // connect SUCCEEDS, the seed reports a no-op, and the repository would
        // go Ready with no history — #380 re-entered through the back door.
        assert!(seed_left_repository_empty(true, false, 0));
        // A repository that actually holds history is fine.
        assert!(!seed_left_repository_empty(true, false, 1));
        // The explicit opt-out switches the guard off, exactly as it does for
        // the source-side empty gate.
        assert!(!seed_left_repository_empty(true, true, 0));
        // And a bootstrap with no seed armed is untouched: an empty repository
        // is the ordinary outcome of `spec.create.enabled` on a fresh backend.
        assert!(!seed_left_repository_empty(false, false, 0));
    }

    #[test]
    fn the_left_empty_failure_is_retryable_under_the_resume_contract() {
        // Reclassified once `resume` existed: with it, the next bootstrap
        // re-runs the copy against the already-initialized backend instead of
        // reporting the no-op, so an interrupted seed CONVERGES. The remedy
        // changed with the classification — it must no longer tell anyone to
        // delete a repository kopiur is about to finish filling.
        let r = BootstrapResult::seed_left_empty();
        assert!(!r.success);
        assert!(r.seed.is_none());
        let f = r.failure.expect("failure block");
        assert_eq!(f.kopia_error_class, SEED_LEFT_EMPTY_CLASS);
        assert!(f.retry_recommended);
        // what
        assert!(f.message.contains("ZERO snapshots"), "{}", f.message);
        // why it happened
        assert!(f.message.contains("earlier seed attempt"), "{}", f.message);
        // fix: kopiur retries itself; the operator tunes the deadline
        assert!(f.message.contains("resumes the copy"), "{}", f.message);
        assert!(f.message.contains("activeDeadlineSeconds"), "{}", f.message);
        assert!(
            f.message.contains("spec.seed.allowEmptySource: true"),
            "{}",
            f.message
        );
        // The OLD remedy must be gone: with resume, deleting the backend
        // repository destroys the partial copy the next attempt would finish.
        // Matched on the imperative, since the new text mentions deletion only
        // to forbid it ("nothing at the backend should be deleted").
        assert!(
            !f.message.contains("delete the half-initialized repository"),
            "the pre-resume remedy is still in the message: {}",
            f.message
        );
        assert!(
            f.message
                .contains("nothing at the backend should be deleted"),
            "{}",
            f.message
        );
        assert!(!f.message.contains("   "), "{}", f.message);
    }

    #[test]
    fn every_seed_failure_message_says_what_why_and_how_to_fix_it() {
        let not_found = BootstrapResult::seed_source_not_found()
            .failure
            .unwrap()
            .message;
        // what
        assert!(
            not_found.contains("holds no kopia repository"),
            "{not_found}"
        );
        // fix: the overwhelmingly common cause is a wrong prefix
        assert!(not_found.contains("spec.seed.from"), "{not_found}");
        assert!(not_found.contains("prefix"), "{not_found}");

        let empty = BootstrapResult::seed_source_empty()
            .failure
            .unwrap()
            .message;
        // what + why it is refused rather than silently accepted
        assert!(empty.contains("zero snapshots"), "{empty}");
        assert!(empty.contains("Ready"), "{empty}");
        // fix: name the exact override field
        assert!(
            empty.contains("spec.seed.allowEmptySource: true"),
            "{empty}"
        );

        let incomplete = BootstrapResult::seed_incomplete(2, 5, "mydb@prod:/pvc@t")
            .failure
            .unwrap()
            .message;
        // what: the counts and a sample of what did not arrive
        assert!(incomplete.contains("2 of 5"), "{incomplete}");
        assert!(incomplete.contains("mydb@prod:/pvc@t"), "{incomplete}");
        // why exit 0 was not the success signal
        assert!(incomplete.contains("exits 0"), "{incomplete}");
        // fix: retrying is safe and copies only the remainder
        assert!(incomplete.contains("idempotent"), "{incomplete}");

        // Authored through a bash heredoc, not a Python one — the wrapped-
        // whitespace defect that hit the C1 error literals would show up here.
        for msg in [not_found, empty, incomplete] {
            assert!(!msg.contains("   "), "wrapped source whitespace: {msg}");
        }
    }

    #[test]
    fn a_mover_error_becomes_a_bootstrap_failure_without_inventing_a_class() {
        // The seed paths can fail outside a kopia invocation (credential
        // staging, a missing source password). Class, retry hint and message
        // must all come from the typed error rather than a hand-written trio
        // that could drift.
        let err = crate::error::MoverError::SeedPasswordMissing {
            env_key: crate::env::SEED_KOPIA_PASSWORD,
        };
        let r = BootstrapResult::from_mover_error(&err);
        assert!(!r.success);
        assert!(r.seed.is_none());
        let f = r.failure.unwrap();
        assert_eq!(f.kopia_error_class, err.kopia_class().as_str());
        assert_eq!(f.retry_recommended, err.retry_recommended());
        assert_eq!(f.message, err.to_string());
    }

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
        let r = BootstrapResult::ready(
            true,
            Some("abc".into()),
            Some(3),
            vec![],
            false,
            0,
            Some(42),
        );
        let back: BootstrapResult =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert!(back.success && back.created);
        assert_eq!(back.unique_id.as_deref(), Some("abc"));
        assert_eq!(back.snapshot_count, Some(3));
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
            Some(raw.len() as i64),
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
            Some(slim.len() as i64),
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
        let result =
            BootstrapResult::ready(false, Some("u".into()), Some(200), slim, false, 0, Some(1));
        // A deliberately tiny budget forces trimming.
        let guarded = enforce_result_size_budget(result, 4_096);
        assert!(guarded.snapshots.len() < 200, "must have dropped entries");
        assert!(guarded.snapshots_truncated, "must flag the truncation");
        // The authoritative count is never rewritten by the size guard.
        assert_eq!(guarded.snapshot_count, Some(200));
        assert!(
            serde_json::to_string(&guarded).unwrap().len() <= 4_096 || guarded.snapshots.is_empty(),
            "trims until it fits (or nothing is left to drop)"
        );
    }

    #[test]
    fn enforce_result_size_budget_is_a_noop_under_budget() {
        let slim = vec![slim_catalog_entry(fat_entry("k1"))];
        let result =
            BootstrapResult::ready(false, Some("u".into()), Some(1), slim, false, 0, Some(1));
        let before = result.clone();
        let guarded = enforce_result_size_budget(result, RESULT_SIZE_BUDGET_BYTES);
        assert_eq!(
            guarded, before,
            "an in-budget result must be left untouched"
        );
    }
}
