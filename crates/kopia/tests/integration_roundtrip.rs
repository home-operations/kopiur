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
    ConnectOptions, ConnectSpec, KopiaClient, MaintenanceMode, PolicyArgs, RestoreOptions,
    SyncToOptions, VerifyOptions,
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
    // `--sources <identity>` (issue #250) is exercised here too: the pinned kopia
    // binary must both PARSE the flag and MATCH the snapshot recorded under this
    // identity (verify exits 0 — it found the source and its blobs are intact).
    client
        .snapshot_verify(&VerifyOptions {
            sources: vec![identity.to_string()],
            verify_files_percent: Some(100),
            file_parallelism: Some(2),
            file_queue_length: Some(100),
            ..Default::default()
        })
        .await
        .expect("verify with --sources scope + file-parallelism/file-queue-length");

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

/// The permanent guard for every kopia tag-mechanics assumption the `kopiur-meta`
/// snapshot-metadata feature rests on, proven against the pinned binary (each point
/// was also manually verified against this exact kopia 0.23.1 while writing the
/// test):
///
/// 1. **First-colon split**: the legacy CLI string `kopiur:config:<name>` is stored
///    under manifest key `tag:kopiur` with value `config:<name>`. `kopiur` is
///    therefore an occupied CLI key, and any new kopiur tag MUST use a colon-free
///    CLI key (`kopiur-meta`).
/// 2. **`tag:` manifest prefix**: user tags land in the manifest `tags` map with a
///    `tag:` key prefix ([`kopiur_kopia::user_tags`] strips it), and a JSON-blob
///    value — colons, braces, quotes — survives verbatim.
/// 3. **Duplicate keys fail the create outright** ("Duplicate tag <key> found"):
///    two CLI tags whose strings collide on the first-colon key (`kopiur:meta` +
///    `kopiur:config:x` → both key `kopiur`) BREAK THE BACKUP. This is why
///    `Snapshot.spec.tags` reserves the `kopiur` prefix and forbids colons.
/// 4. **`snapshot list --json` emits the tags map** — the read-back path the
///    catalog scan depends on exists.
/// 5. **Tags survive manifest rewrites**: `snapshot pin` (which CHANGES the
///    manifest id) and full maintenance both preserve the tags map.
#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn tag_mechanics_first_colon_split_duplicate_keys_and_rewrite_survival() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(source_dir.path().join("a.txt"), b"tagged\n").unwrap();

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

    // The exact two tags the controller writes: the legacy `kopiur:config` string and
    // the kopiur-meta JSON blob.
    let meta_json = r#"{"schema":1,"uid":1000,"gid":1000,"fsGroup":65532}"#;
    let mut tags = BTreeMap::new();
    tags.insert("kopiur:config".to_string(), "mypolicy".to_string());
    tags.insert("kopiur-meta".to_string(), meta_json.to_string());
    let created = client
        .snapshot_create(
            source_dir.path().to_str().unwrap(),
            &tags,
            Some("taguser@taghost:/data"),
        )
        .await
        .expect("create with the legacy tag + the meta tag must not collide");

    // (1)+(2): first-colon split and the `tag:` manifest prefix, on the create echo.
    assert_eq!(
        created.tags.get("tag:kopiur").map(String::as_str),
        Some("config:mypolicy"),
        "kopia splits the CLI string on the FIRST colon: key `kopiur`, value \
         `config:mypolicy`, stored under the `tag:` manifest prefix; got {:?}",
        created.tags
    );
    assert_eq!(
        created.tags.get("tag:kopiur-meta").map(String::as_str),
        Some(meta_json),
        "the JSON blob value must survive verbatim"
    );
    let stripped = kopiur_kopia::user_tags(&created.tags);
    assert_eq!(
        stripped.get("kopiur-meta").map(String::as_str),
        Some(meta_json)
    );

    // (3): a second CLI tag colliding on the first-colon key fails the create.
    let mut colliding = BTreeMap::new();
    colliding.insert("kopiur".to_string(), "meta".to_string());
    colliding.insert("kopiur:config".to_string(), "x".to_string());
    let err = client
        .snapshot_create(
            source_dir.path().to_str().unwrap(),
            &colliding,
            Some("taguser@taghost:/data"),
        )
        .await;
    assert!(
        err.is_err(),
        "two CLI tags colliding on the first-colon key (`kopiur`) must fail the \
         create — this is the backup-breaking hazard the reserved-prefix validator \
         exists to prevent"
    );

    // (4): the list read-back path carries the tags.
    let list = client.snapshot_list(None).await.expect("snapshot list");
    let entry = list
        .iter()
        .find(|e| e.id == created.id)
        .expect("created snapshot present in list");
    assert_eq!(entry.tags, created.tags, "list must echo the manifest tags");

    // (5): pin REWRITES the manifest (the id changes) — tags must survive, and the
    // reconciler-visible lesson is that the id is NOT stable across a pin.
    client
        .snapshot_pin(&created.id, "protected")
        .await
        .expect("pin");
    let pinned_list = client
        .snapshot_list(Some(&created.source))
        .await
        .expect("list after pin");
    let pinned = pinned_list
        .first()
        .expect("snapshot still present after pin");
    assert_ne!(
        pinned.id, created.id,
        "pin rewrites the manifest under a NEW id (reconcilers must re-resolve)"
    );
    assert_eq!(
        pinned.tags, created.tags,
        "tags must survive the pin's manifest rewrite"
    );

    // ...and full maintenance must not strip them either.
    client
        .maintenance_run(MaintenanceMode::Full)
        .await
        .expect("full maintenance");
    let after = client
        .snapshot_list(Some(&created.source))
        .await
        .expect("list after maintenance");
    assert_eq!(
        after.first().map(|e| &e.tags),
        Some(&created.tags),
        "tags must survive full maintenance"
    );
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

/// Real-kopia guard for the blob-mode `spec.seed` mechanic (issue #380): the
/// two behaviors the seeding mover rests on, neither of which the pure unit
/// tests can prove.
///
/// 1. **`repository sync-to` works from a `--readonly` connect.** The seed
///    opens its source read-only on purpose — the mirror may still be another
///    cluster's live off-site copy — and kopia persists that read-only bit into
///    the client config, so every later invocation on the connection is
///    structurally unable to write. If a kopia release ever refused `sync-to`
///    on such a connection, blob seeding would break with a confusing
///    "storage is read-only", and the fallback (a normal connect; sync-to never
///    writes the source either way) would have to be adopted deliberately.
///    Verified by hand against kopia 0.23.1 during the C2 spike; pinned here so
///    it stays verified.
///
/// 2. **The copy carries the SOURCE's `kopia.maintenance` owner.** This is the
///    entire justification for the seeded-restamp rule
///    (`maintenance_restamp_target`'s `seeded` arm): a blob-seeded repository
///    arrives owned by the operator of the cluster the mirror came from, which
///    — under `RestampPolicy::OwnFormatsOnly`, forced whenever
///    `identityDefaults.cluster` is set — the ordinary self-heal would refuse to
///    touch as "foreign", leaving maintenance yielding forever on a repository
///    nobody else can claim. If a future kopia stopped copying that blob, the
///    unconditional restamp would become dead code rather than a live fix, and
///    this assertion is what would say so.
#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn sync_to_seeds_an_empty_backend_from_a_readonly_source_connect() {
    let source_repo_dir = tempfile::tempdir().unwrap();
    let seeded_repo_dir = tempfile::tempdir().unwrap();
    let admin_config_dir = tempfile::tempdir().unwrap();
    let ro_config_dir = tempfile::tempdir().unwrap();
    let seeded_config_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(source_dir.path().join("a.txt"), b"seed me\n").unwrap();
    let source_spec = ConnectSpec::Filesystem {
        path: source_repo_dir.path().to_path_buf(),
    };

    // (a) The "mirror": a repository with one snapshot, plus a maintenance
    // owner standing in for the now-dead source cluster's operator.
    let admin = isolated_client(admin_config_dir.path());
    admin
        .repository_create(&source_spec, Default::default(), &Default::default())
        .await
        .expect("mirror repository create");
    admin
        .snapshot_create(
            source_dir.path().to_str().unwrap(),
            &BTreeMap::new(),
            Some("seeduser@seedhost:/data"),
        )
        .await
        .expect("mirror snapshot create");
    admin
        .maintenance_set_owner("kopiur@kopiur-dead-cluster-repo")
        .await
        .expect("stamp the source cluster's maintenance owner");

    // (b) Re-open the mirror READ-ONLY under its own config, with credentials
    // persisted — the exact connect `seed_connect_source` performs.
    let source_ro = isolated_client(ro_config_dir.path());
    source_ro
        .repository_connect_with(
            &source_spec,
            Default::default(),
            ConnectOptions {
                readonly: true,
                persist_credentials: true,
            },
        )
        .await
        .expect("read-only connect to the seed source");

    // (c) THE MECHANIC: sync-to an EMPTY destination directory from that
    // read-only connection, with the two options `SeedOpSpec::sync_options`
    // fixes — `--no-must-exist` (initializing the destination IS the point) and
    // no `--delete`.
    source_ro
        .repository_sync_to(
            &ConnectSpec::Filesystem {
                path: seeded_repo_dir.path().to_path_buf(),
            },
            &SyncToOptions {
                must_exist: Some(false),
                delete_extra: false,
                parallel: Some(2),
                ..Default::default()
            },
        )
        .await
        .expect("sync-to from a READ-ONLY source connect must succeed (issue #380)");

    // (d) The seeded backend is a working repository holding the mirror's
    // history — proving the copy happened, not merely that kopia exited 0.
    let seeded = isolated_client(seeded_config_dir.path());
    seeded
        .repository_connect(
            &ConnectSpec::Filesystem {
                path: seeded_repo_dir.path().to_path_buf(),
            },
            Default::default(),
        )
        .await
        .expect("connect to the seeded repository");
    let list = seeded
        .snapshot_list_all()
        .await
        .expect("list the seeded repository");
    assert_eq!(
        list.len(),
        1,
        "the seeded repository must hold the mirror's snapshot"
    );
    assert_eq!(
        list[0].source.identity(),
        "seeduser@seedhost:/data",
        "seeding must preserve the snapshot's identity, or the history it \
         restores is unreachable by identity/fromPolicy"
    );

    // (d2) …and the UNFILTERED listing sees it too, from a client connected as
    // some entirely different `user@host`. Two things rest on this and would
    // fail silently if a kopia release ever scoped a source-less
    // `snapshot list` to the connected identity the way its `--all` help text
    // suggests:
    //
    //   * the bootstrap catalog scan (`snapshot_list(None)`) would report ZERO
    //     for every seeded repository and materialize no discovered Snapshot
    //     CRs — the seeded history would be invisible to kopiur;
    //   * the `spec.seed` empty-repository backstop would have to be the only
    //     thing standing between that and a `Ready` empty repository.
    //
    // The backstop deliberately calls `snapshot_list_all` so it survives such a
    // change; this assertion is what would tell us the change happened.
    let unfiltered = seeded
        .snapshot_list(None)
        .await
        .expect("list the seeded repository without --all");
    assert_eq!(
        unfiltered.len(),
        1,
        "a source-less `snapshot list` must still return FOREIGN identities \
         (kopia 0.23.1: `--all` only narrows when a <source> positional is \
         given). If this fails, kopia changed: the bootstrap catalog scan now \
         misses every seeded snapshot and must move to snapshot_list_all"
    );
    assert_eq!(unfiltered[0].source.identity(), "seeduser@seedhost:/data");

    // (e) ...and it arrived owned by the DEAD cluster's operator, which is why
    // the mover restamps a just-seeded repository unconditionally.
    let info = seeded
        .maintenance_info()
        .await
        .expect("read the seeded repository's maintenance owner");
    assert_eq!(
        info.owner, "kopiur@kopiur-dead-cluster-repo",
        "a blob seed carries the SOURCE's maintenance owner; the mover's \
         seeded-restamp rule exists to replace it"
    );
}
