//! Re-resolving a snapshot's CURRENT manifest id after kopia rewrites it.
//!
//! kopia assigns a **new** manifest id whenever a snapshot manifest is edited:
//! `snapshot pin`/`unpin` both call kopia's `UpdateSnapshot`, which *saves a new
//! manifest and deletes the old manifest id*. The id stored in
//! `Snapshot.status.snapshot.kopiaSnapshotID` at create time therefore goes
//! stale the moment the snapshot is pinned — pointing at a deleted manifest. A
//! restore/delete/`fromPolicy` keyed on the stale id then fails (`content … not
//! found`) or, worse, the finalizer delete silently no-ops and orphans the live
//! pinned manifest.
//!
//! A snapshot's *source path* and *start time* survive the manifest rewrite, so
//! we re-match on those. A path alone is NOT globally unique, though: the same
//! PVC subpath (e.g. `/pvc/data`) repeats across namespaces, and — in a shared
//! repository — across clusters, so a path-only match can select (and, in the
//! delete path, DELETE) a different identity's snapshot. When the caller has a
//! recorded `username`/`hostname` we require both to match too, in addition to
//! path; when it doesn't (anchors captured before identity was recorded), we
//! fall back to the previous path-only behavior exactly. This module is **pure
//! data** (no kopia subprocess, no kube) so the matching policy is
//! unit-testable on its own.

use chrono::{DateTime, Utc};
use kopiur_kopia::SnapshotListEntry;

/// Whether `entry` is a candidate for `source_path`, optionally narrowed by
/// `identity` (`username`, `hostname`). `identity: None` matches on path
/// alone — the legacy behavior for anchors captured before identity was
/// recorded. Shared by [`match_current_manifest`] and the mover's
/// latest-snapshot pick (verify/restore-heal), which wants the single newest
/// candidate rather than an ambiguity-refusing unique match.
pub fn matches_source(
    entry: &SnapshotListEntry,
    source_path: &str,
    identity: Option<(&str, &str)>,
) -> bool {
    entry.source.path == source_path
        && match identity {
            Some((username, hostname)) => {
                entry.source.user_name == username && entry.source.host == hostname
            }
            None => true,
        }
}

/// Find the entry in `entries` corresponding to the snapshot identified by
/// `source_path` (+ `start_time` when known, + `identity` — `(username,
/// hostname)` — when known), after its manifest id may have changed. Returns
/// `None` when the match would be ambiguous — so a caller never re-stamps,
/// restores, or deletes the WRONG snapshot:
///
/// - `identity`, when `Some`, is applied together with the path: a candidate
///   must share the exact kopia `user_name`/`host`, not just the path — two
///   sources sharing a path (the same PVC subpath across namespaces, or,
///   in a shared repository, across clusters) must never cross-match.
///   `identity: None` (anchors captured before identity was recorded)
///   preserves the previous path-only behavior exactly.
/// - With a `start_time` anchor: the unique entry whose source path (+
///   identity) matches AND whose start time equals the anchor (compared as
///   instants, not strings, so `Z` vs `+00:00` formatting never matters).
/// - Without a `start_time` anchor: the single path(+identity) match, or
///   `None` if several snapshots share that combination (we cannot tell which
///   one is meant).
///
/// An empty `source_path` always yields `None` (nothing to anchor on).
pub fn match_current_manifest<'a>(
    entries: &'a [SnapshotListEntry],
    source_path: &str,
    start_time: Option<DateTime<Utc>>,
    identity: Option<(&str, &str)>,
) -> Option<&'a SnapshotListEntry> {
    if source_path.is_empty() {
        return None;
    }
    let candidates = entries
        .iter()
        .filter(|e| matches_source(e, source_path, identity));
    match start_time {
        Some(anchor) => unique(candidates.filter(|e| e.start_time == anchor)),
        // No disambiguator: only safe when exactly one snapshot has this
        // path(+identity).
        None => unique(candidates),
    }
}

/// The single element of `it`, or `None` if it is empty or has more than one.
fn unique<'a, I>(mut it: I) -> Option<&'a SnapshotListEntry>
where
    I: Iterator<Item = &'a SnapshotListEntry>,
{
    let first = it.next()?;
    if it.next().is_some() {
        None
    } else {
        Some(first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_kopia::{SnapshotListEntry, SnapshotSource};

    fn entry(id: &str, path: &str, start: &str) -> SnapshotListEntry {
        entry_identity(id, path, start, "home-wyoming-whisper", "home")
    }

    /// Like `entry`, but with an explicit `user_name`/`host` — for the
    /// identity-aware matching tests, where two entries share a `path` but
    /// belong to different identities.
    fn entry_identity(
        id: &str,
        path: &str,
        start: &str,
        user_name: &str,
        host: &str,
    ) -> SnapshotListEntry {
        SnapshotListEntry {
            id: id.to_string(),
            source: SnapshotSource {
                host: host.into(),
                user_name: user_name.into(),
                path: path.to_string(),
            },
            description: String::new(),
            start_time: DateTime::parse_from_rfc3339(start)
                .unwrap()
                .with_timezone(&Utc),
            end_time: DateTime::parse_from_rfc3339(start)
                .unwrap()
                .with_timezone(&Utc),
            stats: Default::default(),
            root_entry: None,
            retention_reason: vec![],
        }
    }

    fn anchor(s: &str) -> Option<DateTime<Utc>> {
        Some(DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc))
    }

    #[test]
    fn matches_new_id_by_path_and_start_when_old_id_is_gone() {
        // The pre-pin id `3ad0…` is gone; the post-pin manifest `b203…` has the
        // SAME source path + start time. We must resolve to `b203…`.
        let entries = vec![
            entry("b2037e14", "/pvc/wyoming-whisper", "2026-06-19T05:54:19Z"),
            entry("unrelated", "/pvc/other", "2026-06-19T05:54:19Z"),
        ];
        let m = match_current_manifest(
            &entries,
            "/pvc/wyoming-whisper",
            anchor("2026-06-19T05:54:19Z"),
            None,
        );
        assert_eq!(m.map(|e| e.id.as_str()), Some("b2037e14"));
    }

    #[test]
    fn start_time_disambiguates_same_path_snapshots() {
        let entries = vec![
            entry("older", "/pvc/db", "2026-06-18T00:00:00Z"),
            entry("newer", "/pvc/db", "2026-06-19T00:00:00Z"),
        ];
        let m = match_current_manifest(&entries, "/pvc/db", anchor("2026-06-18T00:00:00Z"), None);
        assert_eq!(m.map(|e| e.id.as_str()), Some("older"));
    }

    #[test]
    fn timezone_format_does_not_affect_instant_equality() {
        // CR stores `+00:00`, kopia list emits `Z`; both are the same instant.
        let entries = vec![entry("b2037e14", "/pvc/db", "2026-06-19T05:54:19Z")];
        let m = match_current_manifest(
            &entries,
            "/pvc/db",
            anchor("2026-06-19T05:54:19+00:00"),
            None,
        );
        assert_eq!(m.map(|e| e.id.as_str()), Some("b2037e14"));
    }

    #[test]
    fn no_anchor_single_path_candidate_is_picked() {
        let entries = vec![entry("only", "/pvc/db", "2026-06-19T00:00:00Z")];
        let m = match_current_manifest(&entries, "/pvc/db", None, None);
        assert_eq!(m.map(|e| e.id.as_str()), Some("only"));
    }

    #[test]
    fn no_anchor_multiple_candidates_refuses_to_guess() {
        // Without a start-time anchor we must NOT mis-stamp/mis-delete.
        let entries = vec![
            entry("a", "/pvc/db", "2026-06-18T00:00:00Z"),
            entry("b", "/pvc/db", "2026-06-19T00:00:00Z"),
        ];
        assert!(match_current_manifest(&entries, "/pvc/db", None, None).is_none());
    }

    #[test]
    fn empty_source_path_never_matches() {
        let entries = vec![entry("a", "/pvc/db", "2026-06-19T00:00:00Z")];
        assert!(
            match_current_manifest(&entries, "", anchor("2026-06-19T00:00:00Z"), None).is_none()
        );
    }

    #[test]
    fn no_match_returns_none() {
        let entries = vec![entry("a", "/pvc/db", "2026-06-19T00:00:00Z")];
        assert!(match_current_manifest(&entries, "/pvc/missing", None, None).is_none());
        // Anchor present but no entry at that instant.
        assert!(
            match_current_manifest(&entries, "/pvc/db", anchor("2020-01-01T00:00:00Z"), None)
                .is_none()
        );
    }

    // --- identity-aware matching (M0a: cross-identity delete/verify hazard) ---

    #[test]
    fn different_hostname_same_path_yields_no_match() {
        // THE regression test: another identity's snapshot at the SAME path
        // (e.g. the same PVC subpath written from a different namespace, or —
        // in a shared repository — a different cluster) must never be picked.
        // Without this filter the delete self-heal (`resolve_live_id`) would
        // delete SOMEONE ELSE'S snapshot.
        let entries = vec![entry_identity(
            "theirs",
            "/pvc/data",
            "2026-06-19T00:00:00Z",
            "app",
            "cluster-b",
        )];
        let m = match_current_manifest(
            &entries,
            "/pvc/data",
            anchor("2026-06-19T00:00:00Z"),
            Some(("app", "cluster-a")),
        );
        assert!(m.is_none());
    }

    #[test]
    fn same_path_same_identity_matches() {
        let entries = vec![entry_identity(
            "mine",
            "/pvc/data",
            "2026-06-19T00:00:00Z",
            "app",
            "cluster-a",
        )];
        let m = match_current_manifest(
            &entries,
            "/pvc/data",
            anchor("2026-06-19T00:00:00Z"),
            Some(("app", "cluster-a")),
        );
        assert_eq!(m.map(|e| e.id.as_str()), Some("mine"));
    }

    #[test]
    fn identity_disambiguates_same_path_without_start_time() {
        // Two different identities wrote the same path; no start-time anchor
        // is available (an older recorded Snapshot), but the identity alone
        // narrows the candidates to a unique match.
        let entries = vec![
            entry_identity(
                "mine",
                "/pvc/data",
                "2026-06-19T00:00:00Z",
                "app",
                "cluster-a",
            ),
            entry_identity(
                "theirs",
                "/pvc/data",
                "2026-06-18T00:00:00Z",
                "app",
                "cluster-b",
            ),
        ];
        let m = match_current_manifest(&entries, "/pvc/data", None, Some(("app", "cluster-a")));
        assert_eq!(m.map(|e| e.id.as_str()), Some("mine"));
    }

    #[test]
    fn no_identity_given_preserves_legacy_path_only_match() {
        // Anchors captured before this fix carry no identity — behavior must
        // be exactly as before: the single path match, regardless of whose
        // snapshot it actually is.
        let entries = vec![entry_identity(
            "only",
            "/pvc/db",
            "2026-06-19T00:00:00Z",
            "someone-else",
            "other-cluster",
        )];
        let m = match_current_manifest(&entries, "/pvc/db", anchor("2026-06-19T00:00:00Z"), None);
        assert_eq!(m.map(|e| e.id.as_str()), Some("only"));
    }

    #[test]
    fn identity_given_but_multiple_candidates_still_refuses_to_guess() {
        // Ambiguity behavior is unchanged by identity filtering: two
        // snapshots sharing BOTH path and identity, with no start-time
        // anchor, must still refuse rather than guess.
        let entries = vec![
            entry_identity("a", "/pvc/db", "2026-06-18T00:00:00Z", "app", "cluster-a"),
            entry_identity("b", "/pvc/db", "2026-06-19T00:00:00Z", "app", "cluster-a"),
        ];
        assert!(
            match_current_manifest(&entries, "/pvc/db", None, Some(("app", "cluster-a"))).is_none()
        );
    }
}
