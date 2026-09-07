//! Real-kopia integration coverage for the stdin-fed snapshot path.
//!
//! Gated behind the `integration` feature and `#[ignore]` by default so the
//! hermetic `cargo test` never invokes the real binary. Run with:
//!
//! ```text
//! cargo test -p kopiur-kopia --features integration --test integration_stdin
//! ```
//!
//! These are the tests that pin the CORRECTNESS CLAIM the whole stream-source
//! feature rests on: a producer that fails leaves NO snapshot, even when it had
//! already written every byte. If kopia ever changed so that it committed a
//! manifest before stdin reached EOF, `abort_after_full_payload_leaves_no_snapshot`
//! is what would catch it — and a silent regression there would mean shipping
//! truncated database dumps as successful backups.

#![cfg(unix)]

use std::collections::BTreeMap;

use kopiur_kopia::{
    ConnectSpec, KopiaClient, SnapshotCreateOptions, SnapshotCreateOutcome, SnapshotSource,
    StdinOutcome,
};
use tokio::io::AsyncWriteExt;

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

async fn fresh_repo(repo: &std::path::Path, config: &std::path::Path) -> KopiaClient {
    let client = isolated_client(config);
    client
        .repository_create(
            &ConnectSpec::Filesystem {
                path: repo.to_path_buf(),
            },
            Default::default(),
            &Default::default(),
        )
        .await
        .expect("create repository");
    client
}

/// Commit path: bytes in, one virtual file out, byte-identical on the way back.
#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn stdin_snapshot_roundtrips_byte_identical() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let client = fresh_repo(repo_dir.path(), config_dir.path()).await;

    // Deliberately larger than any pipe buffer, and not newline-structured, so a
    // chunking or text-mangling bug shows up as a hash mismatch.
    let payload: Vec<u8> = (0..(3 * 1024 * 1024u32)).map(|i| (i % 251) as u8).collect();
    let expected = payload.clone();

    let outcome = client
        .snapshot_create_stdin_outcome_with(
            "/stream/dump.sql",
            "dump.sql",
            &BTreeMap::new(),
            Some("tester@host:/stream/dump.sql"),
            &SnapshotCreateOptions::default(),
            async |stdin: &mut tokio::process::ChildStdin| {
                stdin.write_all(&payload).await.expect("write payload");
                Ok(StdinOutcome::Commit)
            },
        )
        .await
        .expect("stdin snapshot should succeed");

    let result = match outcome {
        SnapshotCreateOutcome::Created(r) => *r,
        SnapshotCreateOutcome::Unchanged => panic!("a stdin snapshot is never 'unchanged'"),
    };
    // One regular file inside a virtual directory — the shape the restore path relies on.
    let root = result
        .root_entry
        .expect("a stdin snapshot always has a root entry");
    assert_eq!(root.summary.as_ref().map(|s| s.files), Some(1));

    let root_obj = root.obj.clone();
    let mut got: Vec<u8> = Vec::new();
    client
        .show_to(&format!("{root_obj}/dump.sql"), &mut got)
        .await
        .expect("show the stored file");
    assert_eq!(got.len(), expected.len(), "restored length differs");
    assert!(
        got == expected,
        "restored bytes differ from what was streamed in"
    );
}

/// THE load-bearing test. The producer writes the COMPLETE payload and only then
/// reports failure. Because kopia's stdin is still open, aborting must leave no
/// manifest at all — "all bytes delivered" must never be the commit point.
#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn abort_after_full_payload_leaves_no_snapshot() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let client = fresh_repo(repo_dir.path(), config_dir.path()).await;

    let before = client.snapshot_list_all().await.expect("list before").len();

    let err = client
        .snapshot_create_stdin_outcome_with(
            "/stream/dump.sql",
            "dump.sql",
            &BTreeMap::new(),
            Some("tester@host:/stream/dump.sql"),
            &SnapshotCreateOptions::default(),
            async |stdin: &mut tokio::process::ChildStdin| {
                stdin
                    .write_all(b"a complete and perfectly valid looking dump\n")
                    .await
                    .expect("write payload");
                // stdin is deliberately NOT closed: the runner owns it and closes it
                // only AFTER killing kopia, which is what guarantees kopia never
                // reaches EOF and never writes a manifest.
                Ok(StdinOutcome::Abort)
            },
        )
        .await
        .expect_err("an aborted producer must fail the snapshot");

    assert!(
        matches!(err, kopiur_kopia::KopiaError::StdinProducerFailed { .. }),
        "expected StdinProducerFailed, got {err:?}"
    );

    let after = client.snapshot_list_all().await.expect("list after");
    assert_eq!(
        after.len(),
        before,
        "aborting the producer must leave NO snapshot manifest behind, found {after:#?}"
    );
    assert!(
        !after.iter().any(|e| e.source.path == "/stream/dump.sql"),
        "no snapshot may exist for the aborted source"
    );
}

/// A producer whose own error propagates: same guarantee, different arm of the
/// runner (`Err` rather than `Ok(Abort)`).
#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn producer_error_propagates_and_leaves_no_snapshot() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let client = fresh_repo(repo_dir.path(), config_dir.path()).await;

    let err = client
        .snapshot_create_stdin_outcome_with(
            "/stream/dump.sql",
            "dump.sql",
            &BTreeMap::new(),
            Some("tester@host:/stream/dump.sql"),
            &SnapshotCreateOptions::default(),
            async |stdin: &mut tokio::process::ChildStdin| {
                stdin.write_all(b"partial").await.expect("write");
                Err(kopiur_kopia::KopiaError::EmptyOutput {
                    context: "synthetic producer failure".to_string(),
                    stderr_tail: String::new(),
                })
            },
        )
        .await
        .expect_err("the producer's own error must fail the snapshot");
    assert!(matches!(err, kopiur_kopia::KopiaError::EmptyOutput { .. }));

    let after = client.snapshot_list_all().await.expect("list after");
    assert!(after.is_empty(), "no snapshot may exist, found {after:#?}");
}

/// `--stdin-file` must reach the argv, and the recorded identity must be the
/// operator-resolved one rather than the mover pod's ambient user/host.
#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn stdin_snapshot_records_the_overridden_identity() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let client = fresh_repo(repo_dir.path(), config_dir.path()).await;

    client
        .snapshot_create_stdin_outcome_with(
            "/stream/postgres.sql",
            "postgres.sql",
            &BTreeMap::new(),
            Some("bundlecop@k7:/stream/postgres.sql"),
            &SnapshotCreateOptions::default(),
            async |stdin: &mut tokio::process::ChildStdin| {
                stdin.write_all(b"SELECT 1;\n").await.unwrap();
                Ok(StdinOutcome::Commit)
            },
        )
        .await
        .expect("snapshot");

    let listed = client
        .snapshot_list(Some(&SnapshotSource {
            user_name: "bundlecop".into(),
            host: "k7".into(),
            path: "/stream/postgres.sql".into(),
        }))
        .await
        .expect("list by identity");
    assert_eq!(
        listed.len(),
        1,
        "expected exactly one snapshot: {listed:#?}"
    );
    assert_eq!(listed[0].source.user_name, "bundlecop");
    assert_eq!(listed[0].source.host, "k7");
}
