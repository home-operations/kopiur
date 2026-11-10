//! Real-kopia pin for the quick-verify coverage environment (issue #646).
//!
//! Gated like `crates/kopia/tests/integration_roundtrip.rs`: `#[ignore]` unless
//! the `integration` feature is on, so hermetic `cargo test` never invokes the
//! real binary. Needs only a `kopia` on `PATH` (mise pins it) — no cluster.
//!
//! ```text
//! cargo test -p kopiur-mover --features integration --test integration_verify_coverage
//! ```
//!
//! kopia's manifest `stats.fileCount` counts only files it READ during the
//! snapshot. Snapshot an unchanged directory twice and the second manifest is
//! 100% cache hits — `fileCount: 0` — so a quick verify that reported that
//! field as `stats.files` failed the default `stats.files > 0` predicate
//! forever on a healthy repository. This test drives the REAL binary into that
//! exact shape, so a kopia upgrade that changes the counters, or a regression
//! back to the raw field, fails here rather than in a user's cluster.

#![cfg(unix)]

use std::collections::BTreeMap;

use kopiur_kopia::{ConnectSpec, KopiaClient, VerifyOptions};
use kopiur_mover::verify::quick_coverage;

/// The documented default predicate — the one #646 reported failing.
const DEFAULT_SUCCESS_EXPR: &str = "stats.files > 0 && stats.errors == 0";

/// A client whose env isolates kopia state inside `config_dir`, so the test
/// never touches the user's real `~/.config/kopia`.
fn isolated_client(config_dir: &std::path::Path) -> KopiaClient {
    KopiaClient::builder()
        .binary("kopia")
        .env("KOPIA_PASSWORD", "test1234")
        .env(
            "KOPIA_CONFIG_PATH",
            config_dir.join("repository.config").display().to_string(),
        )
        .env(
            "KOPIA_CACHE_DIRECTORY",
            config_dir.join("cache").display().to_string(),
        )
        .env(
            "KOPIA_LOG_DIR",
            config_dir.join("logs").display().to_string(),
        )
        .env("KOPIA_CHECK_FOR_UPDATES", "false")
        .build()
}

#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn a_snapshot_of_an_unchanged_source_passes_the_default_quick_verify_predicate() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("a.txt"), b"hello kopiur\n").unwrap();
    std::fs::create_dir(source_dir.path().join("sub")).unwrap();
    std::fs::write(source_dir.path().join("sub/b.bin"), [0u8, 1, 2, 3, 255]).unwrap();

    let client = isolated_client(config_dir.path());
    client
        .repository_create(
            &ConnectSpec::Filesystem {
                path: repo_dir.path().to_path_buf(),
            },
            Default::default(),
            &Default::default(),
        )
        .await
        .expect("repository create");

    let identity = "verifyuser@verifyhost:/data";
    let source = source_dir.path().to_str().unwrap();
    let first = client
        .snapshot_create(source, &BTreeMap::new(), Some(identity))
        .await
        .expect("first snapshot");
    // Same bytes, same mtimes: every file is a cache hit. kopia's default
    // policy still writes a manifest (`ignoreIdenticalSnapshots` is off).
    let second = client
        .snapshot_create(source, &BTreeMap::new(), Some(identity))
        .await
        .expect("second snapshot of the unchanged source");
    assert_ne!(first.id, second.id, "kopia wrote a second manifest");

    let list = client.snapshot_list(None).await.expect("snapshot list");
    let cached = list
        .iter()
        .find(|e| e.id == second.id)
        .cloned()
        .expect("second manifest listed");

    // Pin the trap with the real binary: the raw counter excludes cached
    // files. If kopia ever starts counting them, this documents the change
    // rather than silently passing for a different reason.
    assert_eq!(
        cached.stats.file_count, 0,
        "kopia's fileCount counts only files it re-read; manifest stats: {:?}",
        cached.stats
    );
    assert_eq!(cached.stats.cached_files, 2, "both files came from cache");
    assert_eq!(cached.files_in_snapshot(), 2);

    // The quick tier verifies this snapshot cleanly...
    client
        .snapshot_verify(&VerifyOptions {
            sources: vec![identity.to_string()],
            ..Default::default()
        })
        .await
        .expect("quick verify of the cached snapshot");

    // ...and its successExpr environment reports the files the snapshot holds,
    // so the default predicate passes.
    let (stats, id) = quick_coverage(cached);
    assert_eq!(id, second.id);
    assert_eq!(stats.files, 2, "files IN the snapshot, cached or not");
    assert_eq!(stats.bytes, 13 + 5);
    let inputs = kopiur_api::SuccessExprInputs {
        stats,
        snapshot: [("id".to_string(), id)].into(),
        restored: None,
        tier: "quick".to_string(),
        _marker: std::marker::PhantomData,
    };
    assert!(
        kopiur_api::eval_success_expr(DEFAULT_SUCCESS_EXPR, &inputs).expect("evaluates"),
        "the default successExpr must pass on an unchanged source (#646)"
    );

    // The rootEntry-less fallback (hashed + cached) agrees with the tree count
    // on the same real manifest.
    let mut slim = list
        .into_iter()
        .find(|e| e.id == second.id)
        .expect("second manifest listed");
    slim.root_entry = None;
    assert_eq!(slim.files_in_snapshot(), 2);
}
