//! Real-kopia integration test for the `E2E (populator-fanout)` flake: a READ-WRITE
//! kopia connection over a READ-ONLY repository mount fails a plain `snapshot list` as
//! soon as the repository holds enough uncompacted manifest contents.
//!
//! Gated behind the `integration` feature and `#[ignore]` by default so the hermetic
//! `cargo test` never invokes the real binary:
//!
//! ```text
//! cargo test -p kopiur-kopia --features integration --test integration_readonly_manifest_compaction
//! ```
//!
//! ## The mechanism this pins (kopia 0.23.1, `repo/manifest`)
//!
//! Loading the manifest set — which `snapshot list`, `snapshot restore` and every
//! other manifest read does first — AUTO-COMPACTS the manifest contents once there
//! are `autoCompactionContentCountDefault` (16) or more of them, and compaction
//! WRITES a pack blob and a session marker. kopia skips it for exactly one kind of
//! client: one opened read-only (`maybeCompactLocked`: "Don't attempt to compact
//! manifests if the repo was opened in read only mode since we'll just end up
//! failing").
//!
//! The restore mover mounted a filesystem repository's volume READ-ONLY but
//! connected kopia READ-WRITE. kopia therefore believed it could compact, tried to,
//! and the whole read failed with `mkdir /repo/q8a: read-only file system` — a
//! `MoverJobFailed` restore of a perfectly good snapshot, captured verbatim in CI.
//! Whether it hit was down to how many manifest contents the repository's writers
//! had left uncompacted when the restore loaded, which is why it flaked; #700 made
//! every snapshot write more manifest contents (a `policy delete` + `policy set` per
//! source per run), which is why it started flaking then.
//!
//! What it asserts, for one repository driven to exactly the threshold:
//!
//! * a read-write connection over the read-only repository FAILS `snapshot list`, and
//!   the failure is the compaction write hitting the read-only filesystem — the
//!   premise of the bug, so a kopia that stops auto-compacting on load fails here
//!   loudly instead of leaving the fix unexplained;
//! * a `--readonly` connection over the SAME read-only repository lists the snapshot
//!   — the fix the restore mover now uses.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use kopiur_kopia::{CacheTuning, ConnectSpec, KopiaClient, PolicyArgs};

const PASSWORD: &str = "readonly-compaction-pass";
const IDENTITY_USER: &str = "rouser";
const IDENTITY_HOST: &str = "rohost";

/// A client whose kopia state is isolated inside `config_dir`, same pattern as
/// `integration_epoch_floors.rs`.
fn isolated_client(config_dir: &Path) -> KopiaClient {
    KopiaClient::builder()
        .binary("kopia")
        .env("KOPIA_PASSWORD", PASSWORD)
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

/// Recursively set `mode` on every directory and `file_mode` on every file under
/// `root` (and on `root` itself). Directories need `x` to stay traversable.
fn chmod_tree(root: &Path, dir_mode: u32, file_mode: u32) {
    let meta = std::fs::metadata(root).expect("stat");
    if meta.is_dir() {
        for entry in std::fs::read_dir(root).expect("read_dir") {
            chmod_tree(&entry.expect("dir entry").path(), dir_mode, file_mode);
        }
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(dir_mode))
            .expect("chmod dir");
    } else {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(file_mode))
            .expect("chmod file");
    }
}

/// Make the repository read-only the way a `readOnly: true` volume mount presents
/// it, and prove it took: as root, mode bits are not enforced, and a test that
/// silently ran with a writable repository would pass for the wrong reason.
fn make_read_only(repo: &Path) {
    chmod_tree(repo, 0o555, 0o444);
    let probe = repo.join(".write-probe");
    assert!(
        std::fs::write(&probe, b"x").is_err(),
        "the repository directory is still writable after chmod — this test must run as a \
         non-root user, or it cannot simulate the read-only volume mount it is about"
    );
}

fn make_writable(repo: &Path) {
    chmod_tree(repo, 0o755, 0o644);
}

/// Whether `err` is kopia failing a write against the read-only repository.
fn is_read_only_write_failure(err: &kopiur_kopia::KopiaError) -> bool {
    err.to_string().contains("read-only file system")
        || err.to_string().contains("permission denied")
}

#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn a_read_write_connection_over_a_read_only_repository_fails_a_list_that_a_readonly_connection_serves()
 {
    let repo = tempfile::tempdir().unwrap();
    let rw_cfg = tempfile::tempdir().unwrap();
    let ro_cfg = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("marker.txt"), "alpha-bytes").unwrap();
    let spec = ConnectSpec::Filesystem {
        path: repo.path().to_path_buf(),
    };

    // The writer (a snapshot mover, in production) and the reader (the restore
    // mover), on separate kopia configs. The reader connects while the
    // repository is still writable — the connect is not what this is about.
    let writer = isolated_client(rw_cfg.path());
    writer
        .repository_create(&spec, CacheTuning::default(), &Default::default())
        .await
        .expect("repository create");
    let override_source = format!("{IDENTITY_USER}@{IDENTITY_HOST}:{}", data.path().display());
    writer
        .snapshot_create(
            &data.path().display().to_string(),
            &Default::default(),
            Some(&override_source),
        )
        .await
        .expect("seed snapshot");
    let reader = isolated_client(ro_cfg.path());
    reader
        .repository_connect_readonly(&spec, CacheTuning::default())
        .await
        .expect("read-only connect");

    // Drive the repository's manifest-content count up ONE write at a time, and
    // after each write probe it the way the restore mover did: a read-write
    // `snapshot list` over the read-only repository. Every `policy set` is itself
    // a read-write load (it compacts at the threshold), so the count can never
    // overshoot unobserved — the first probe that sees the threshold is the one
    // that fails. The bound is generous: kopia's threshold is 16 contents.
    let identity = format!("{IDENTITY_USER}@{IDENTITY_HOST}");
    let mut hit = None;
    for write in 1..=40_i64 {
        writer
            .policy_set(
                &identity,
                &PolicyArgs {
                    // A different value every write, so every call really writes
                    // a new policy manifest content.
                    keep_latest: Some(1000 + write),
                    ..Default::default()
                },
            )
            .await
            .expect("policy set");
        make_read_only(repo.path());
        let probe = writer.snapshot_list(None).await;
        if let Err(e) = probe {
            hit = Some((write, e));
            break;
        }
        make_writable(repo.path());
    }
    let (writes, err) = hit.expect(
        "a read-write `snapshot list` over a read-only repository never failed: kopia no \
         longer auto-compacts manifests on load, so the populator-fanout root cause this \
         test pins has changed — re-derive why the restore mover connects --readonly",
    );
    eprintln!("read-write list over the read-only repository failed after {writes} writes: {err}");
    assert!(
        is_read_only_write_failure(&err) && err.to_string().contains("compact"),
        "the read-write list must fail on the AUTO-COMPACTION write hitting the read-only \
         repository (after {writes} writes), got: {err}"
    );

    // The fix: a `--readonly` connection skips the compaction and serves the same
    // read over the same read-only repository, still sitting at the threshold.
    let listed = reader.snapshot_list(None).await.unwrap_or_else(|e| {
        panic!("a --readonly connection must list a read-only repository: {e}")
    });
    make_writable(repo.path());
    assert_eq!(
        listed.len(),
        1,
        "the read-only connection must see the seeded snapshot: {listed:?}"
    );
}
