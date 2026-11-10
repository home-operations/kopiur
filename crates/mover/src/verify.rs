//! **Pure** pieces of the verify flow, split out of the binary so they are
//! unit-testable and reachable from the real-kopia integration tests
//! (`tests/integration_verify_coverage.rs`).

/// **Pure.** The quick tier's `successExpr` environment derived from the newest
/// snapshot manifest for its identity: `(stats, snapshot id)`.
///
/// `kopia snapshot verify` reports no machine-readable file/byte counts of its
/// own, so a healthy quick verify of a non-empty snapshot satisfies the common
/// `stats.files > 0` predicate from the manifest instead of always failing on a
/// hardcoded 0. `stats.files` is the number of files the snapshot HOLDS
/// ([`kopiur_kopia::SnapshotListEntry::files_in_snapshot`]), including files
/// kopia served from cache. `errors` is 0 — a passing verify found no integrity errors.
/// Counts saturate rather than wrap, so an absurd manifest cannot make a
/// predicate read a small number.
///
/// Deliberately takes the manifest that WAS found. "No manifest for this
/// identity" is not an environment to report, it is a terminal verdict
/// (`MoverError::VerifyNoSnapshot`): `kopia snapshot verify --sources` exits 0
/// when its filter matches ZERO manifests, so reporting success there stamps
/// `status.lastVerified` for a source path the repository holds no snapshot for
/// — the #456 false pass.
pub fn quick_coverage(entry: kopiur_kopia::SnapshotListEntry) -> (kopiur_api::VerifyStats, String) {
    (
        kopiur_api::VerifyStats {
            // Files IN the snapshot — never `stats.file_count`, which counts
            // only files kopia re-read and is 0 for an unchanged source (#646).
            files: i64::try_from(entry.files_in_snapshot()).unwrap_or(i64::MAX),
            bytes: i64::try_from(entry.stats.total_size).unwrap_or(i64::MAX),
            errors: 0,
        },
        entry.id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The documented default predicate (`docs/.../backups.mdx`), the one #646
    /// reported failing forever on an unchanged source.
    const DEFAULT_SUCCESS_EXPR: &str = "stats.files > 0 && stats.errors == 0";

    /// A manifest entry with the given counts, for [`quick_coverage`].
    fn manifest(id: &str, files: u64, bytes: u64) -> kopiur_kopia::SnapshotListEntry {
        let mut e: kopiur_kopia::SnapshotListEntry = serde_json::from_value(serde_json::json!({
            "id": id,
            "source": { "host": "h", "userName": "u", "path": "/pvc/data" },
            "startTime": "2026-08-01T00:00:00Z",
            "endTime": "2026-08-01T00:01:00Z",
        }))
        .expect("typed manifest entry");
        e.stats.file_count = files;
        e.stats.total_size = bytes;
        e
    }

    /// The exact manifest shape kopia 0.23 writes for a snapshot of an
    /// UNCHANGED source (captured from a real repository): every file served
    /// from cache, so `fileCount: 0`, `cachedFiles: N`, while
    /// `rootEntry.summ.files` still holds the true total.
    fn fully_cached_manifest(with_root_summary: bool) -> kopiur_kopia::SnapshotListEntry {
        let mut json = serde_json::json!({
            "id": "kcached",
            "source": { "host": "h", "userName": "u", "path": "/pvc/data" },
            "startTime": "2026-11-09T23:39:05Z",
            "endTime": "2026-11-09T23:39:06Z",
            "stats": {
                "totalSize": 6, "fileCount": 0, "cachedFiles": 2,
                "nonCachedFiles": 0, "dirCount": 1, "errorCount": 0
            },
        });
        if with_root_summary {
            json["rootEntry"] = serde_json::json!({
                "name": "data", "type": "d", "obj": "k510d",
                "summ": { "size": 6, "files": 2, "dirs": 1, "numFailed": 0 }
            });
        }
        serde_json::from_value(json).expect("typed manifest entry")
    }

    /// Evaluate `expr` over the quick tier's environment, exactly as the
    /// verify flow does.
    fn passes(expr: &str, stats: kopiur_api::VerifyStats, id: &str) -> bool {
        let inputs = kopiur_api::SuccessExprInputs {
            stats,
            snapshot: [("id".to_string(), id.to_string())].into(),
            restored: None,
            tier: "quick".to_string(),
            _marker: std::marker::PhantomData,
        };
        kopiur_api::eval_success_expr(expr, &inputs).expect("default successExpr evaluates")
    }

    #[test]
    fn quick_coverage_reports_the_manifest_counts_and_saturates() {
        let (stats, id) = quick_coverage(manifest("k1", 42, 4096));
        assert_eq!(id, "k1");
        assert_eq!(stats.files, 42);
        assert_eq!(stats.bytes, 4096);
        assert_eq!(
            stats.errors, 0,
            "a passing quick verify found no integrity errors"
        );
        // Saturate, never wrap: a wrapped count could make `stats.files > 0`
        // read false (or a byte budget read small) on an absurd manifest.
        let (stats, _) = quick_coverage(manifest("k2", u64::MAX, u64::MAX));
        assert_eq!(stats.files, i64::MAX);
        assert_eq!(stats.bytes, i64::MAX);
    }

    #[test]
    fn a_fully_cached_snapshot_covers_every_file_and_passes_the_default_predicate() {
        // #646: kopia's `fileCount` counts only files it READ. A snapshot of an
        // unchanged source is 100% cache hits (`fileCount: 0`), and the quick
        // tier used to report that as `stats.files == 0`, so the default
        // `stats.files > 0 && stats.errors == 0` failed forever on a healthy
        // repository. `stats.files` means "files in the verified snapshot".
        for with_root_summary in [true, false] {
            let (stats, id) = quick_coverage(fully_cached_manifest(with_root_summary));
            assert_eq!(
                stats.files, 2,
                "files in the snapshot, not files kopia re-read \
                 (rootEntry.summ present: {with_root_summary})"
            );
            assert_eq!(stats.bytes, 6);
            assert!(
                passes(DEFAULT_SUCCESS_EXPR, stats, &id),
                "the default successExpr must pass on an unchanged source \
                 (rootEntry.summ present: {with_root_summary})"
            );
        }
    }

    #[test]
    fn a_genuinely_empty_snapshot_still_fails_the_default_predicate() {
        // The predicate exists to kill the silent "0 files" success — the fix
        // must not turn an EMPTY snapshot into a pass.
        let (stats, id) = quick_coverage(manifest("kempty", 0, 0));
        assert_eq!(stats.files, 0);
        assert!(!passes(DEFAULT_SUCCESS_EXPR, stats, &id));
    }
}
