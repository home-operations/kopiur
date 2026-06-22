//! Pure snapshot-selection helpers shared by the controller and the mover.
//!
//! Restore resolution picks a snapshot from a `kopia snapshot list` by an
//! optional point-in-time cutoff (`asOf`) and a zero-based `offset` (0 = newest).
//! These two operations are pure functions over an already-fetched, newest-first
//! list, so they live here (next to [`crate::SnapshotListEntry`]) rather than in
//! either binary — the mover resolves object-store restores in-Job and the
//! controller's tests exercise the same logic. The RFC3339 parse of the `asOf`
//! string is the caller's concern (the admission webhook validates it; callers
//! re-parse defensively) so these stay infallible.

use chrono::{DateTime, Utc};

use crate::SnapshotListEntry;

/// Keep only snapshots taken at or before `cutoff` (point-in-time selection),
/// preserving order so it composes with [`pick_offset`]: filter first, then
/// offset ("the previous one as of last Tuesday"). `None` keeps the full list.
pub fn filter_as_of(
    mut snapshots: Vec<SnapshotListEntry>,
    cutoff: Option<DateTime<Utc>>,
) -> Vec<SnapshotListEntry> {
    if let Some(cutoff) = cutoff {
        snapshots.retain(|e| e.end_time <= cutoff);
    }
    snapshots
}

/// Pick the snapshot at `offset` (0 = newest) from a newest-first list. A
/// negative offset clamps to the newest rather than panicking; an out-of-range
/// offset returns `None` (the caller applies `onMissingSnapshot`).
pub fn pick_offset(snapshots: Vec<SnapshotListEntry>, offset: i64) -> Option<SnapshotListEntry> {
    let idx = offset.max(0) as usize;
    snapshots.into_iter().nth(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list_entry(id: &str, end_time: &str) -> SnapshotListEntry {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "source": { "host": "h", "userName": "u", "path": "/data" },
            "startTime": end_time,
            "endTime": end_time,
        }))
        .expect("valid SnapshotListEntry")
    }

    /// Three snapshots, newest-first (the order the list is sorted into).
    fn three_snapshots() -> Vec<SnapshotListEntry> {
        vec![
            list_entry("k3", "2026-06-03T00:00:00Z"),
            list_entry("k2", "2026-06-02T00:00:00Z"),
            list_entry("k1", "2026-06-01T00:00:00Z"),
        ]
    }

    fn cutoff(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn filter_as_of_keeps_snapshots_at_or_before_the_instant() {
        // A cutoff between k2 and k3 drops k3 (newer than the instant); k2/k1 remain.
        let kept = filter_as_of(three_snapshots(), Some(cutoff("2026-06-02T12:00:00Z")));
        assert_eq!(
            kept.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["k2", "k1"]
        );
        // Exactly AT a snapshot's endTime keeps it ("at or before").
        let kept = filter_as_of(three_snapshots(), Some(cutoff("2026-06-02T00:00:00Z")));
        assert_eq!(kept.first().map(|e| e.id.as_str()), Some("k2"));
        // Before everything → empty (caller applies onMissingSnapshot).
        let kept = filter_as_of(three_snapshots(), Some(cutoff("2026-05-01T00:00:00Z")));
        assert!(kept.is_empty());
        // No cutoff → untouched.
        let kept = filter_as_of(three_snapshots(), None);
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn as_of_composes_with_offset() {
        // "the previous one as of just after k2": asOf drops k3, offset 1 then
        // steps past k2 to k1.
        let kept = filter_as_of(three_snapshots(), Some(cutoff("2026-06-02T12:00:00Z")));
        assert_eq!(pick_offset(kept, 1).map(|e| e.id), Some("k1".to_string()));
    }

    #[test]
    fn pick_offset_zero_is_newest_and_out_of_range_is_none() {
        assert_eq!(
            pick_offset(three_snapshots(), 0).map(|e| e.id),
            Some("k3".to_string())
        );
        assert_eq!(
            pick_offset(three_snapshots(), 2).map(|e| e.id),
            Some("k1".to_string())
        );
        assert_eq!(pick_offset(three_snapshots(), 3).map(|e| e.id), None);
        // A negative offset clamps to newest rather than panicking.
        assert_eq!(
            pick_offset(three_snapshots(), -1).map(|e| e.id),
            Some("k3".to_string())
        );
    }
}
