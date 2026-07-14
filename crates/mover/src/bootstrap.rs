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

use kopiur_api::{HostClass, classify_hostname};
use kopiur_kopia::{KopiaError, KopiaErrorClass, SnapshotListEntry};
use serde::{Deserialize, Serialize};

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
    (listing, truncated, dropped)
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
            failure: None,
        }
    }

    /// A terminal-failure outcome carrying the kopia error class + stderr tail.
    pub fn failed(err: &KopiaError) -> Self {
        BootstrapResult {
            success: false,
            created: false,
            unique_id: None,
            snapshot_count: 0,
            snapshots: Vec::new(),
            snapshots_truncated: false,
            foreign_suffix_dropped: 0,
            index_blob_count: None,
            failure: Some(failure_block_from_kopia(err)),
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
        BootstrapResult {
            success: false,
            created: false,
            unique_id: None,
            snapshot_count: 0,
            snapshots: Vec::new(),
            snapshots_truncated: false,
            foreign_suffix_dropped: 0,
            index_blob_count: None,
            failure: Some(FailureBlock {
                kopia_error_class: REPOSITORY_NOT_INITIALIZED_CLASS.to_string(),
                message: REPOSITORY_NOT_INITIALIZED_MESSAGE.to_string(),
                stderr_tail: None,
                exit_code: None,
                retry_recommended: false,
            }),
        }
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
    auto_create
        && !matches!(
            class,
            KopiaErrorClass::AuthFailure
                | KopiaErrorClass::Locked
                | KopiaErrorClass::AccessDenied
                | KopiaErrorClass::PermissionDenied
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
}
