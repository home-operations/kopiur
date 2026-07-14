//! Real kopia filesystem round-trip integration test.
//!
//! Gated behind the `integration` feature and `#[ignore]` by default so the
//! hermetic `cargo test` never invokes the real binary (the `integration`
//! feature lifts the `#[ignore]`, so it is not needed on the command line).
//! Run with:
//!
//! ```text
//! cargo test -p kopiur-kopia --features integration
//! ```
//!
//! It creates a filesystem repo in a tempdir, snapshots a tempdir with known
//! content, lists (asserts the snapshot appears), restores to another tempdir,
//! and asserts byte-identical content.

#![cfg(unix)]

use std::collections::BTreeMap;

use kopiur_kopia::{
    ConnectSpec, KopiaClient, MaintenanceMode, PolicyArgs, RestoreOptions, SyncToOptions,
    VerifyOptions,
};

/// Build a client whose env isolates kopia state inside `config_dir` so the
/// test never touches the user's real `~/.config/kopia`.
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
        // Suppress the GitHub update check via env (it's a per-subcommand flag,
        // not a global one, so it can't go in common_args).
        .env("KOPIA_CHECK_FOR_UPDATES", "false")
        .build()
}

#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn filesystem_roundtrip() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let restore_dir = tempfile::tempdir().unwrap();

    // Known content.
    std::fs::write(source_dir.path().join("a.txt"), b"hello kopiur\n").unwrap();
    std::fs::create_dir(source_dir.path().join("sub")).unwrap();
    std::fs::write(source_dir.path().join("sub/b.bin"), [0u8, 1, 2, 3, 255]).unwrap();

    let client = isolated_client(config_dir.path());

    // Create the repository.
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

    // Snapshot with a tag.
    let mut tags = BTreeMap::new();
    tags.insert("test".to_string(), "roundtrip".to_string());
    let created = client
        .snapshot_create(
            source_dir.path().to_str().unwrap(),
            &tags,
            Some("testuser@testhost:/data"),
        )
        .await
        .expect("snapshot create");
    assert!(!created.id.is_empty());
    assert_eq!(
        created.source.user_name, "testuser",
        "snapshot recorded under the override identity, not the ambient user"
    );
    assert_eq!(created.source.host, "testhost");
    assert_eq!(created.file_count(), 2, "two files snapshotted");
    assert_eq!(created.total_bytes(), 13 + 5);

    // Repository status round-trips.
    let status = client.repository_status().await.expect("repo status");
    assert!(!status.unique_id_hex.is_empty());
    assert_eq!(status.storage.storage_type, "filesystem");

    // List shows the snapshot.
    let list = client.snapshot_list(None).await.expect("snapshot list");
    assert!(
        list.iter().any(|e| e.id == created.id),
        "created snapshot must appear in list"
    );
    let entry = list.iter().find(|e| e.id == created.id).unwrap();
    assert_eq!(entry.stats.file_count, 2);

    // Filtered list by the created snapshot's source identity also finds it.
    let filtered = client
        .snapshot_list(Some(&created.source))
        .await
        .expect("filtered list");
    assert!(filtered.iter().any(|e| e.id == created.id));

    // Maintenance info parses against the real repo.
    let info = client.maintenance_info().await.expect("maintenance info");
    assert!(!info.owner.is_empty());

    // Restore to a fresh dir.
    client
        .snapshot_restore(&created.id, restore_dir.path().to_str().unwrap())
        .await
        .expect("snapshot restore");

    // Byte-identical assertions.
    let a = std::fs::read(restore_dir.path().join("a.txt")).expect("a.txt restored");
    assert_eq!(a, b"hello kopiur\n");
    let b = std::fs::read(restore_dir.path().join("sub/b.bin")).expect("b.bin restored");
    assert_eq!(b, &[0u8, 1, 2, 3, 255]);

    // A quick maintenance pass succeeds.
    client
        .maintenance_run(MaintenanceMode::Quick)
        .await
        .expect("quick maintenance");

    // Delete the snapshot, then confirm it's gone from the list.
    client
        .snapshot_delete(&created.id)
        .await
        .expect("snapshot delete");
    let after = client.snapshot_list(None).await.expect("list after delete");
    assert!(
        !after.iter().any(|e| e.id == created.id),
        "deleted snapshot must not appear"
    );
}

/// Exercises the broadened verb surface (policy, verify, estimate, pin, restore
/// options, validate-provider) against a real filesystem repo. Proves the args
/// we build are accepted by kopia 0.23, not just shaped correctly.
#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn verbs_roundtrip() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let restore_dir = tempfile::tempdir().unwrap();

    std::fs::write(source_dir.path().join("keep.txt"), b"keep me\n").unwrap();
    std::fs::write(source_dir.path().join("skip.tmp"), b"scratch\n").unwrap();

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

    // validate-provider preflight succeeds against a freshly created repo.
    client
        .repository_validate_provider()
        .await
        .expect("validate-provider");

    let identity = "verbuser@verbhost:/data";

    // Apply a policy (compression + ignore glob) before snapshotting.
    client
        .policy_set(
            identity,
            &PolicyArgs {
                compression: Some("zstd".into()),
                ignore: vec!["*.tmp".into()],
                ..Default::default()
            },
        )
        .await
        .expect("policy set");

    // policy show reflects the compression we set.
    let shown = client.policy_show(identity).await.expect("policy show");
    assert!(
        shown.to_string().contains("zstd"),
        "policy show should reflect zstd compression, got {shown}"
    );

    // Estimate runs cleanly.
    client
        .snapshot_estimate(source_dir.path().to_str().unwrap())
        .await
        .expect("snapshot estimate");

    // Snapshot; the ignore policy should drop skip.tmp (1 file, not 2).
    let created = client
        .snapshot_create(
            source_dir.path().to_str().unwrap(),
            &BTreeMap::new(),
            Some(identity),
        )
        .await
        .expect("snapshot create");
    assert_eq!(
        created.file_count(),
        1,
        "ignore policy should exclude *.tmp"
    );

    // Verify integrity (read 100% of files). Also exercises the M3 (issue #216
    // category sweep) tuning knobs `--file-parallelism`/`--file-queue-length`
    // against the real kopia binary — the permanent regression guard that kopia
    // actually accepts these flag forms, not just that the argv shape looks right.
    client
        .snapshot_verify(&VerifyOptions {
            verify_files_percent: Some(100),
            file_parallelism: Some(2),
            file_queue_length: Some(100),
            ..Default::default()
        })
        .await
        .expect("verify with file-parallelism/file-queue-length");

    // Restore honoring options (atomic writes, ignore permission errors).
    client
        .snapshot_restore_with(
            &created.id,
            restore_dir.path().to_str().unwrap(),
            &RestoreOptions {
                ignore_permission_errors: Some(true),
                write_files_atomically: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("restore with options");

    // Pin then unpin the snapshot (protects it from expiry). Pinning rewrites the
    // manifest, so the manifest id changes — re-list to get the current id before
    // unpinning, mirroring what a reconciler must do.
    client
        .snapshot_pin(&created.id, "protected")
        .await
        .expect("pin");
    let pinned = client
        .snapshot_list(Some(&created.source))
        .await
        .expect("list after pin");
    let current_id = pinned
        .first()
        .map(|e| e.id.clone())
        .expect("snapshot still present after pin");
    client
        .snapshot_unpin(&current_id, "protected")
        .await
        .expect("unpin");
    let kept = std::fs::read(restore_dir.path().join("keep.txt")).expect("keep.txt restored");
    assert_eq!(kept, b"keep me\n");
    assert!(
        !restore_dir.path().join("skip.tmp").exists(),
        "ignored file should not be in the snapshot/restore"
    );
}

/// Real-kopia guard for issue #216: `kopia repository sync-to` accepts the
/// tuning flags `sync_to_args` builds — `--parallel`, the `--no-*` tri-state
/// forms (`--no-times`), and the throughput caps — against a real filesystem
/// destination. This is the permanent regression guard for the kingpin
/// flag-form risk noted on `sync_to_args`/`push_tristate`: a bad flag SHAPE
/// (e.g. `--must-exist=false`) would fail here even though the pure arg-builder
/// unit tests only check the argv shape, not that kopia accepts it.
#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn sync_to_accepts_parallel_and_tristate_flags() {
    let source_repo_dir = tempfile::tempdir().unwrap();
    let dest_repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(source_dir.path().join("a.txt"), b"hello sync-to\n").unwrap();

    let client = isolated_client(config_dir.path());
    client
        .repository_create(
            &ConnectSpec::Filesystem {
                path: source_repo_dir.path().to_path_buf(),
            },
            Default::default(),
            &Default::default(),
        )
        .await
        .expect("source repository create");
    client
        .snapshot_create(
            source_dir.path().to_str().unwrap(),
            &BTreeMap::new(),
            Some("syncuser@synchost:/data"),
        )
        .await
        .expect("snapshot create");

    // `--parallel 2 --no-times`: the exact flag combo the brief calls out as
    // smoke-tested against kopia 0.23.1. A wrong flag GRAMMAR (e.g.
    // `--must-exist=false` instead of `--no-must-exist`) fails here with a
    // kopia argv-parse error, not a silently-wrong result.
    client
        .repository_sync_to(
            &ConnectSpec::Filesystem {
                path: dest_repo_dir.path().to_path_buf(),
            },
            &SyncToOptions {
                parallel: Some(2),
                times: Some(false),
                must_exist: Some(false),
                update: Some(false),
                max_upload_speed_bytes_per_second: Some(1_000_000),
                ..Default::default()
            },
        )
        .await
        .expect("sync-to with --parallel 2 --no-times should succeed");

    // The destination is now itself a connectable repository with the mirrored
    // snapshot — proving the copy (not just a successful exit code) happened.
    let dest_config_dir = config_dir.path().join("dest");
    std::fs::create_dir_all(&dest_config_dir).unwrap();
    let dest_client = isolated_client(&dest_config_dir);
    dest_client
        .repository_connect(
            &ConnectSpec::Filesystem {
                path: dest_repo_dir.path().to_path_buf(),
            },
            Default::default(),
        )
        .await
        .expect("connect to the sync-to destination");
    let list = dest_client
        .snapshot_list(None)
        .await
        .expect("list destination snapshots");
    assert_eq!(
        list.len(),
        1,
        "the mirrored snapshot must appear at the destination"
    );
}

/// Real-kopia guard for the M2 restore flag sweep (issue #216 gap analysis):
/// `kopia snapshot restore` accepts `--parallel 2`, `--skip-times`, and —
/// critically — `--delete-extra`, the flag `enableFileDeletion` was
/// **previously unable to reach at all** (a silent no-op bug: the CRD field
/// existed, but `kopiur_kopia::RestoreOptions` had no `delete_extra` field and
/// `restore_args` never emitted the flag). Restoring into a target that
/// already contains a file NOT present in the snapshot proves `--delete-extra`
/// actually deletes it — not just that kopia accepted the flag on argv.
///
/// Per the semantic gotcha: `--delete-extra` only takes effect if the restore
/// itself succeeds, and kopia's `overwrite-directories` default is already
/// `true`, so this test must NOT pass `--no-overwrite-directories` (that would
/// make the restore fail against the pre-populated, non-empty target).
#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn restore_accepts_m2_flag_sweep_and_deletes_extra_files() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let restore_dir = tempfile::tempdir().unwrap();

    std::fs::write(source_dir.path().join("keep.txt"), b"from the snapshot\n").unwrap();

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
    let created = client
        .snapshot_create(
            source_dir.path().to_str().unwrap(),
            &BTreeMap::new(),
            Some("m2user@m2host:/data"),
        )
        .await
        .expect("snapshot create");

    // Pre-populate the restore target with a file NOT in the snapshot — the
    // thing `--delete-extra` is supposed to remove. Without `enableFileDeletion`
    // wired up, this file would silently survive the restore (the additive-only
    // bug this milestone fixes).
    std::fs::write(restore_dir.path().join("stale.txt"), b"leftover\n").unwrap();

    client
        .snapshot_restore_with(
            &created.id,
            restore_dir.path().to_str().unwrap(),
            &RestoreOptions {
                parallel: Some(2),
                skip_times: Some(true),
                delete_extra: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("restore with --parallel 2 --skip-times --delete-extra should succeed");

    // The snapshot's content is present...
    let kept = std::fs::read(restore_dir.path().join("keep.txt")).expect("keep.txt restored");
    assert_eq!(kept, b"from the snapshot\n");
    // ...and the pre-existing extra file is GONE — proving kopia actually
    // accepted and acted on `--delete-extra`, not just that argv parsed.
    assert!(
        !restore_dir.path().join("stale.txt").exists(),
        "stale.txt should have been deleted by --delete-extra"
    );
}

#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn snapshot_create_accepts_m4_flag_sweep_and_records_the_description() {
    // M4 flag sweep (issue #216 category sweep): `snapshot create --fail-fast
    // --upload-limit-mb <n> --description <text>` is accepted by real kopia
    // (smoke-tested against 0.23.1 in the design doc; this is the permanent
    // guard), and the description round-trips onto the created snapshot's
    // JSON (not just accepted argv).
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(source_dir.path().join("a.txt"), b"m4 flag sweep\n").unwrap();

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

    let created = client
        .snapshot_create_with(
            source_dir.path().to_str().unwrap(),
            &BTreeMap::new(),
            Some("m4user@m4host:/data"),
            &kopiur_kopia::SnapshotCreateOptions {
                fail_fast: Some(true),
                upload_limit_mb: Some(100),
                description: Some("m4 flag sweep smoke test".to_string()),
            },
        )
        .await
        .expect("snapshot create --fail-fast --upload-limit-mb 100 --description should succeed");
    assert_eq!(created.description, "m4 flag sweep smoke test");

    // `snapshot list` shows the same description was actually persisted on
    // the manifest, not just echoed back by `create`'s own JSON.
    let list = client
        .snapshot_list(None)
        .await
        .expect("snapshot list after m4 create");
    let entry = list
        .iter()
        .find(|e| e.id == created.id)
        .expect("created snapshot present in list");
    assert_eq!(entry.description, "m4 flag sweep smoke test");
}

/// M0b (confirmed data-loss bug): real-kopia proof that pinning the six
/// `--keep-*` fields to a very large value at the IDENTITY scope, before the
/// first `snapshot create`, neutralizes kopia's own create-time retention.
///
/// kopia's `snapshot create` unconditionally applies the *source's* retention
/// policy after every create — even under `--override-source`
/// (`policy.ApplyRetentionPolicy`, kopia's `cli/command_snapshot_create.go`) —
/// and with no policy set, kopia's OWN defaults apply (`keep-latest: 10`
/// among them; `snapshot/policy/retention_policy.go`). Twelve creates under
/// one identity would, on kopia's defaults, prune the two oldest manifests
/// the moment the 11th/12th is created (manually verified against this exact
/// pinned kopia 0.23.1 binary while writing this test: 12 creates with no
/// policy override left only 10 manifests; with this policy applied first,
/// all 12 survived). Those pruned manifests are exactly what a Kopiur
/// `Snapshot` CR — whose GFS window may be wider than kopia's defaults — would
/// still reference, so this is Kopiur's permanent regression guard that the
/// mover's mandatory identity-scope pin (`kopiur_mover::workspec::KOPIA_KEEP_MAX`)
/// actually works against real kopia, not just that the argv shape looks right.
/// Kept fast: one 1-byte file, twelve creates.
#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn identity_scope_keep_max_pin_survives_twelve_creates() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(source_dir.path().join("a.txt"), b"x").unwrap();

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

    // Mirrors the mover's mandatory identity-scope pin exactly (same six
    // flags, same value) — see `identity_retention_policy` in
    // `crates/mover/src/main.rs` and `KOPIA_KEEP_MAX`'s doc comment.
    const KOPIA_KEEP_MAX: i64 = 2_147_483_647;
    let identity_scope = "retentionuser@retentionhost";
    client
        .policy_set(
            identity_scope,
            &PolicyArgs {
                keep_latest: Some(KOPIA_KEEP_MAX),
                keep_hourly: Some(KOPIA_KEEP_MAX),
                keep_daily: Some(KOPIA_KEEP_MAX),
                keep_weekly: Some(KOPIA_KEEP_MAX),
                keep_monthly: Some(KOPIA_KEEP_MAX),
                keep_annual: Some(KOPIA_KEEP_MAX),
                ..Default::default()
            },
        )
        .await
        .expect("identity-scope keep-* policy set");

    let override_source = format!("{identity_scope}:/data");
    for n in 0..12 {
        client
            .snapshot_create(
                source_dir.path().to_str().unwrap(),
                &BTreeMap::new(),
                Some(&override_source),
            )
            .await
            .unwrap_or_else(|e| panic!("snapshot create #{n} should succeed: {e}"));
    }

    let list = client
        .snapshot_list(None)
        .await
        .expect("snapshot list after twelve creates");
    assert_eq!(
        list.len(),
        12,
        "all 12 manifests must survive with the identity-scope keep-* pin applied; \
         without it kopia's own default keep-latest=10 would have pruned 2, got {list:?}"
    );
}
