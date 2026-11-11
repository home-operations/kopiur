//! Real-kopia proof of #708: `snapshot create --force-hash` is what makes a file
//! rewritten in place WITHOUT a size or mtime change get backed up correctly.
//!
//! Gated like `integration_roundtrip.rs` (the `integration` feature lifts the
//! `#[ignore]`). Run with:
//!
//! ```text
//! cargo test -p kopiur-kopia --features integration --test integration_force_hash
//! ```
//!
//! The test first documents kopia's default behaviour — a same-size, same-mtime
//! rewrite is reused from the previous snapshot unread, so a restore returns the
//! OLD bytes, exactly what MariaDB's `ib_logfile0` hit — and then shows that
//! `force_hash_percent: Some(100)` (from `SnapshotPolicy.spec.upload.
//! forceHashPercent: 100`) re-reads the file and captures the new bytes.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs::{File, FileTimes, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime};

use kopiur_kopia::{ConnectSpec, KopiaClient, SnapshotCreateOptions};

const SOURCE: &str = "dbuser@dbhost:/pvc/db";
const OLD: &[u8] = b"redo log generation 0001: OLD CONTENT\n";
const NEW: &[u8] = b"redo log generation 0002: NEW CONTENT\n";

fn isolated_client(config_dir: &Path) -> KopiaClient {
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

/// Overwrite `path` in place with same-length `bytes`, then put its mtime back
/// to `mtime` — the in-place-writer pattern (pwrite into a preallocated file).
fn rewrite_in_place_keeping_mtime(path: &Path, bytes: &[u8], mtime: SystemTime) {
    let mut f = OpenOptions::new().write(true).open(path).expect("open");
    f.write_all(bytes).expect("rewrite");
    f.set_times(FileTimes::new().set_modified(mtime))
        .expect("restore mtime");
    drop(f);
    let meta = std::fs::metadata(path).expect("metadata");
    assert_eq!(meta.len(), bytes.len() as u64, "size unchanged");
    assert_eq!(meta.modified().expect("mtime"), mtime, "mtime unchanged");
}

async fn snapshot_and_restore(
    client: &KopiaClient,
    source: &Path,
    opts: &SnapshotCreateOptions,
) -> Vec<u8> {
    let created = client
        .snapshot_create_with(
            source.to_str().unwrap(),
            &BTreeMap::new(),
            Some(SOURCE),
            opts,
        )
        .await
        .expect("snapshot create");
    let restore = tempfile::tempdir().unwrap();
    client
        .snapshot_restore(&created.id, restore.path().to_str().unwrap())
        .await
        .expect("restore");
    std::fs::read(restore.path().join("ib_logfile0")).expect("restored file")
}

#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn force_hash_captures_an_in_place_rewrite_that_kopia_otherwise_reuses() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let file = source_dir.path().join("ib_logfile0");
    assert_eq!(OLD.len(), NEW.len(), "the rewrite must keep the size");

    // An mtime well in the past, like a log file the engine preallocated hours ago.
    let mtime = SystemTime::now() - Duration::from_secs(6 * 3600);
    std::fs::write(&file, OLD).unwrap();
    File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_times(FileTimes::new().set_modified(mtime))
        .unwrap();

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

    let default_opts = SnapshotCreateOptions::default();
    assert_eq!(
        snapshot_and_restore(&client, source_dir.path(), &default_opts).await,
        OLD,
        "baseline snapshot"
    );

    // Rewrite in place: same size, same mtime, different bytes.
    rewrite_in_place_keeping_mtime(&file, NEW, mtime);

    // kopia's default: size+mtime match the previous snapshot, so the entry is
    // reused unread and the snapshot silently carries the STALE content. If this
    // ever starts returning NEW, kopia changed its reuse rule and the docs for
    // `forceHashPercent` need revisiting.
    assert_eq!(
        snapshot_and_restore(&client, source_dir.path(), &default_opts).await,
        OLD,
        "without --force-hash kopia reuses the previous content"
    );

    // forceHashPercent: 100 re-reads every file: the new bytes are captured.
    let forced = SnapshotCreateOptions {
        force_hash_percent: Some(100),
        ..Default::default()
    };
    assert_eq!(
        snapshot_and_restore(&client, source_dir.path(), &forced).await,
        NEW,
        "--force-hash=100 must capture the in-place rewrite"
    );
}
