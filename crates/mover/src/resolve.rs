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
//! we re-match on those. This module is **pure data** (no kopia subprocess, no
//! kube) so the matching policy is unit-testable on its own.

use chrono::{DateTime, Utc};
use kopiur_kopia::SnapshotListEntry;

/// Find the entry in `entries` corresponding to the snapshot identified by
/// `source_path` (+ `start_time` when known), after its manifest id may have
/// changed. Returns `None` when the match would be ambiguous — so a caller never
/// re-stamps, restores, or deletes the WRONG snapshot:
///
/// - With a `start_time` anchor: the unique entry whose source path matches AND
///   whose start time equals the anchor (compared as instants, not strings, so
///   `Z` vs `+00:00` formatting never matters).
/// - Without an anchor: the single source-path match, or `None` if several
///   snapshots share that path (we cannot tell which one is meant).
///
/// An empty `source_path` always yields `None` (nothing to anchor on).
pub fn match_current_manifest<'a>(
    entries: &'a [SnapshotListEntry],
    source_path: &str,
    start_time: Option<DateTime<Utc>>,
) -> Option<&'a SnapshotListEntry> {
    if source_path.is_empty() {
        return None;
    }
    let by_path = entries.iter().filter(|e| e.source.path == source_path);
    match start_time {
        Some(anchor) => unique(by_path.filter(|e| e.start_time == anchor)),
        // No disambiguator: only safe when exactly one snapshot has this path.
        None => unique(by_path),
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
        SnapshotListEntry {
            id: id.to_string(),
            source: SnapshotSource {
                host: "home".into(),
                user_name: "home-wyoming-whisper".into(),
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
        );
        assert_eq!(m.map(|e| e.id.as_str()), Some("b2037e14"));
    }

    #[test]
    fn start_time_disambiguates_same_path_snapshots() {
        let entries = vec![
            entry("older", "/pvc/db", "2026-06-18T00:00:00Z"),
            entry("newer", "/pvc/db", "2026-06-19T00:00:00Z"),
        ];
        let m = match_current_manifest(&entries, "/pvc/db", anchor("2026-06-18T00:00:00Z"));
        assert_eq!(m.map(|e| e.id.as_str()), Some("older"));
    }

    #[test]
    fn timezone_format_does_not_affect_instant_equality() {
        // CR stores `+00:00`, kopia list emits `Z`; both are the same instant.
        let entries = vec![entry("b2037e14", "/pvc/db", "2026-06-19T05:54:19Z")];
        let m = match_current_manifest(&entries, "/pvc/db", anchor("2026-06-19T05:54:19+00:00"));
        assert_eq!(m.map(|e| e.id.as_str()), Some("b2037e14"));
    }

    #[test]
    fn no_anchor_single_path_candidate_is_picked() {
        let entries = vec![entry("only", "/pvc/db", "2026-06-19T00:00:00Z")];
        let m = match_current_manifest(&entries, "/pvc/db", None);
        assert_eq!(m.map(|e| e.id.as_str()), Some("only"));
    }

    #[test]
    fn no_anchor_multiple_candidates_refuses_to_guess() {
        // Without a start-time anchor we must NOT mis-stamp/mis-delete.
        let entries = vec![
            entry("a", "/pvc/db", "2026-06-18T00:00:00Z"),
            entry("b", "/pvc/db", "2026-06-19T00:00:00Z"),
        ];
        assert!(match_current_manifest(&entries, "/pvc/db", None).is_none());
    }

    #[test]
    fn empty_source_path_never_matches() {
        let entries = vec![entry("a", "/pvc/db", "2026-06-19T00:00:00Z")];
        assert!(match_current_manifest(&entries, "", anchor("2026-06-19T00:00:00Z")).is_none());
    }

    #[test]
    fn no_match_returns_none() {
        let entries = vec![entry("a", "/pvc/db", "2026-06-19T00:00:00Z")];
        assert!(match_current_manifest(&entries, "/pvc/missing", None).is_none());
        // Anchor present but no entry at that instant.
        assert!(
            match_current_manifest(&entries, "/pvc/db", anchor("2020-01-01T00:00:00Z")).is_none()
        );
    }
}
