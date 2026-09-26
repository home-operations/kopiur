//! Parity pin (issue #476): the mover's full-listing `logical_bytes` and the
//! controller's own `logical_bytes_under_management` must agree.
//!
//! The controller prefers the mover's value for `storageStats.totalSizeBytes`
//! when a new mover supplies it and falls back to computing its own when an old
//! mover does not, so any drift between the two would make the reported size
//! jump with the mover version. This lives in the controller crate because it
//! is the only crate that can see both functions. Hermetic: no cluster.

use kopiur_kopia::{SnapshotListEntry, SnapshotSource, SnapshotStats};

fn entry(id: &str, user: &str, path: &str, secs: i64, size: u64) -> SnapshotListEntry {
    let t0 = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    SnapshotListEntry {
        id: id.into(),
        source: SnapshotSource {
            user_name: user.into(),
            host: "ns".into(),
            path: path.into(),
        },
        description: String::new(),
        start_time: t0 + chrono::Duration::seconds(secs - 30),
        end_time: t0 + chrono::Duration::seconds(secs),
        stats: SnapshotStats {
            total_size: size,
            ..Default::default()
        },
        root_entry: None,
        retention_reason: vec![],
        tags: Default::default(),
        incomplete: None,
    }
}

/// Every shape the two implementations must treat identically: several
/// snapshots per source (newest wins), the same path under two users (one
/// source — both key by path), an `end_time` tie with different sizes (the
/// first-listed wins), a source listed out of time order, and a size past
/// `i64::MAX` (saturates per entry).
fn fixture() -> Vec<SnapshotListEntry> {
    vec![
        entry("a1", "a", "/pvc/a", 10, 100),
        entry("a2", "a", "/pvc/a", 20, 150),
        entry("a0", "a", "/pvc/a", 5, 999),
        entry("b1", "b", "/pvc/a", 15, 7),
        entry("c1", "c", "/pvc/c", 40, 1),
        entry("c2", "c", "/pvc/c", 40, 2),
        entry("d1", "d", "/pvc/d", 50, 3_000_000_000),
        entry("e1", "e", "/pvc/e", 1, 0),
    ]
}

#[test]
fn mover_and_controller_logical_bytes_agree() {
    let listing = fixture();
    let controller = kopiur_controller::repository::logical_bytes_under_management(&listing);
    let mover = kopiur_mover::bootstrap::logical_bytes_under_management(&listing);
    assert_eq!(mover, controller);
    assert_eq!(mover, 150 + 1 + 3_000_000_000, "pinned expected value");

    // And reversed listing order (ties then resolve to the other entry, in both).
    let mut reversed = listing;
    reversed.reverse();
    assert_eq!(
        kopiur_mover::bootstrap::logical_bytes_under_management(&reversed),
        kopiur_controller::repository::logical_bytes_under_management(&reversed),
    );

    // A single out-of-range size saturates identically.
    let huge = vec![entry("h", "h", "/pvc/h", 1, u64::MAX)];
    assert_eq!(
        kopiur_mover::bootstrap::logical_bytes_under_management(&huge),
        kopiur_controller::repository::logical_bytes_under_management(&huge),
    );
    assert_eq!(
        kopiur_controller::repository::logical_bytes_under_management(&[]),
        kopiur_mover::bootstrap::logical_bytes_under_management(&[]),
    );
}

/// Through the real mover pipeline: `prepare_catalog_entries`' `logical_bytes`
/// over the FULL post-prefilter listing equals the controller's computation over
/// that same listing, even when the returned window is capped far below it.
#[test]
fn prepared_catalog_logical_bytes_match_the_controller_over_the_full_listing() {
    let listing = fixture();
    let expected = kopiur_controller::repository::logical_bytes_under_management(&listing);
    let prepared = kopiur_mover::bootstrap::prepare_catalog_entries(listing, None, 1, 1);
    assert_eq!(prepared.entries.len(), 1);
    assert_eq!(prepared.logical_bytes, expected);
}
