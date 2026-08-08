//! Real kopia `snapshot migrate` integration test (M0 spike kept permanent).
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
//! Proves, against the pinned kopia 0.23.1, every mechanic the
//! `SnapshotReplication` mover flow rests on:
//!
//! * `repository connect --readonly --persist-credentials` writes the source
//!   password beside the config (`<config>.kopia-password`).
//! * Password-resolution polarity (verified against kopia 0.23.1 behavior AND
//!   `cli/command_snapshot_migrate.go`): for a NORMAL open (`repository
//!   status`), the env `KOPIA_PASSWORD` wins over the persisted password — a
//!   wrong env password FAILS the open. For `snapshot migrate`'s SOURCE open
//!   it is the reverse: `openSourceRepo` tries the source config's persisted
//!   password FIRST and only falls back to env/flags. The mover's
//!   "persisted password works" probe must therefore run `repository status`
//!   with `KOPIA_PASSWORD` REMOVED (builder `env_remove`), not with a
//!   sentinel value.
//! * `snapshot migrate --source-config` copies foreign identities preserving
//!   `startTime`, is idempotent by (source, startTime), honors `--sources`
//!   selectivity and `--latest-only`, and accepts all three policy-mode
//!   argv shapes (`--no-policies` / `--policies` / `--policies
//!   --overwrite-policies`).
//! * The migrating client must NOT pin `KOPIA_CACHE_DIRECTORY`: the env
//!   override applies to EVERY repository opened by the process, so migrate's
//!   source open reads the DESTINATION's cached format blob from the shared
//!   directory and fails with "invalid repository password".

#![cfg(unix)]

use std::collections::BTreeMap;

use kopiur_kopia::{
    ConnectOptions, ConnectSpec, KopiaClient, MigratePolicies, MigrateSources,
    SnapshotMigrateOptions,
};

const SRC_PASSWORD: &str = "migrate-pass-a";
const DEST_PASSWORD: &str = "migrate-pass-b";

/// Identity of the first migrated source (kopia `user@host:path` triple).
const IDENTITY_ONE: &str = "testuser@testhost:/data";
/// A second identity used to prove `--sources` selectivity.
const IDENTITY_TWO: &str = "otheruser@testhost:/data2";

/// The kopia config file path inside `config_dir` (one config per client, the
/// same layout the replication mover uses: `srepl-source.config` vs
/// `srepl-dest.config`).
fn config_path(config_dir: &std::path::Path) -> String {
    config_dir.join("repository.config").display().to_string()
}

/// Build a client whose env isolates kopia state inside `config_dir` (same
/// pattern as `integration_roundtrip.rs`), with a per-client password.
fn isolated_client(config_dir: &std::path::Path, password: &str) -> KopiaClient {
    KopiaClient::builder()
        .binary("kopia")
        .env("KOPIA_PASSWORD", password)
        .env("KOPIA_CONFIG_PATH", config_path(config_dir))
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

/// Build a DESTINATION client suitable for running `snapshot migrate`: it must
/// NOT pin `KOPIA_CACHE_DIRECTORY` (see the module docs — the env override
/// poisons migrate's source open with the destination's cached format blob).
/// Cache isolation comes from `XDG_CACHE_HOME` instead, which kopia expands
/// per-repository; `env_remove` shields against an ambient
/// `KOPIA_CACHE_DIRECTORY` on the developer's machine.
fn dest_client(config_dir: &std::path::Path, password: &str) -> KopiaClient {
    KopiaClient::builder()
        .binary("kopia")
        .env("KOPIA_PASSWORD", password)
        .env("KOPIA_CONFIG_PATH", config_path(config_dir))
        .env(
            "XDG_CACHE_HOME",
            config_dir.join("xdg-cache").display().to_string(),
        )
        .env_remove("KOPIA_CACHE_DIRECTORY")
        .env(
            "KOPIA_LOG_DIR",
            config_dir.join("logs").display().to_string(),
        )
        .env("KOPIA_CHECK_FOR_UPDATES", "false")
        .build()
}

/// Create a fresh destination filesystem repository and return its client.
async fn create_dest_repo(repo_dir: &std::path::Path, config_dir: &std::path::Path) -> KopiaClient {
    let client = dest_client(config_dir, DEST_PASSWORD);
    client
        .repository_create(
            &ConnectSpec::Filesystem {
                path: repo_dir.to_path_buf(),
            },
            Default::default(),
            &Default::default(),
        )
        .await
        .expect("destination repository create");
    client
}

#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn snapshot_migrate_two_repositories_flow() {
    let src_repo = tempfile::tempdir().unwrap();
    let src_admin_cfg = tempfile::tempdir().unwrap();
    let src_ro_cfg = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let data2_dir = tempfile::tempdir().unwrap();
    let dest1_repo = tempfile::tempdir().unwrap();
    let dest1_cfg = tempfile::tempdir().unwrap();
    let dest2_repo = tempfile::tempdir().unwrap();
    let dest2_cfg = tempfile::tempdir().unwrap();
    let dest3_repo = tempfile::tempdir().unwrap();
    let dest3_cfg = tempfile::tempdir().unwrap();
    let dest4_repo = tempfile::tempdir().unwrap();
    let dest4_cfg = tempfile::tempdir().unwrap();

    // (a) Source repo under password A with one snapshot of IDENTITY_ONE.
    std::fs::write(data_dir.path().join("a.txt"), b"migrate me\n").unwrap();
    let src_admin = isolated_client(src_admin_cfg.path(), SRC_PASSWORD);
    src_admin
        .repository_create(
            &ConnectSpec::Filesystem {
                path: src_repo.path().to_path_buf(),
            },
            Default::default(),
            &Default::default(),
        )
        .await
        .expect("source repository create");
    let created1 = src_admin
        .snapshot_create(
            data_dir.path().to_str().unwrap(),
            &BTreeMap::new(),
            Some(IDENTITY_ONE),
        )
        .await
        .expect("source snapshot create #1");
    assert_eq!(created1.source.identity(), IDENTITY_ONE);

    // (b) Destination repo under a DIFFERENT password B.
    let dest1 = create_dest_repo(dest1_repo.path(), dest1_cfg.path()).await;

    // (c) Reconnect the source under a separate config, read-only, with the
    // password persisted beside the config — the exact connect the replication
    // mover performs for its source.
    let src_ro = isolated_client(src_ro_cfg.path(), SRC_PASSWORD);
    src_ro
        .repository_connect_with(
            &ConnectSpec::Filesystem {
                path: src_repo.path().to_path_buf(),
            },
            Default::default(),
            ConnectOptions {
                readonly: true,
                persist_credentials: true,
            },
        )
        .await
        .expect("source reconnect --readonly --persist-credentials");
    let src_ro_config = config_path(src_ro_cfg.path());
    assert!(
        std::path::Path::new(&format!("{src_ro_config}.kopia-password")).exists(),
        "--persist-credentials must write <config>.kopia-password"
    );

    // (d) Password-resolution probes on the persisted source config.
    //
    // A WRONG env password FAILS a normal open: on the flags path
    // (`repository status`) env `KOPIA_PASSWORD` takes precedence over the
    // persisted password, so a sentinel value does NOT prove anything about
    // the persisted credentials…
    let probe_wrong = isolated_client(src_ro_cfg.path(), "wrong-sentinel");
    let err = probe_wrong
        .repository_status()
        .await
        .expect_err("a wrong env KOPIA_PASSWORD must fail repository status (env wins)");
    assert!(
        err.to_string().contains("invalid repository password"),
        "expected an invalid-password failure, got: {err}"
    );
    // …the probe that DOES prove the persisted password works — the lookup
    // `snapshot migrate` performs FIRST for its source — is a status with
    // KOPIA_PASSWORD absent (builder `env_remove`, shielding against an
    // ambient value too).
    let probe_persisted = KopiaClient::builder()
        .binary("kopia")
        .env_remove("KOPIA_PASSWORD")
        .env("KOPIA_CONFIG_PATH", src_ro_config.clone())
        .env(
            "KOPIA_CACHE_DIRECTORY",
            src_ro_cfg.path().join("cache").display().to_string(),
        )
        .env(
            "KOPIA_LOG_DIR",
            src_ro_cfg.path().join("logs").display().to_string(),
        )
        .env("KOPIA_CHECK_FOR_UPDATES", "false")
        .build();
    let status = probe_persisted
        .repository_status()
        .await
        .expect("with no env password, the persisted password must open the repository");
    assert!(!status.unique_id_hex.is_empty());

    // (e) Migrate everything into dest1. The dest client's own env password
    // (B) is WRONG for the source, so a successful migrate proves the source
    // open used the persisted password, not the env.
    let migrate_all = SnapshotMigrateOptions {
        source_config_path: src_ro_config.clone(),
        sources: MigrateSources::All,
        latest_only: false,
        parallel: Some(2),
        policies: MigratePolicies::None,
    };
    dest1
        .snapshot_migrate(&migrate_all)
        .await
        .expect("snapshot migrate --all --no-policies");
    let src_times_1 = |entries: &[kopiur_kopia::SnapshotListEntry]| {
        let mut t: Vec<_> = entries
            .iter()
            .filter(|e| e.source.identity() == IDENTITY_ONE)
            .map(|e| e.start_time)
            .collect();
        t.sort();
        t
    };
    let source_list = src_admin
        .snapshot_list(Some(&created1.source))
        .await
        .expect("source list");
    let dest_list = dest1.snapshot_list_all().await.expect("dest1 list --all");
    assert_eq!(dest_list.len(), 1, "exactly one migrated snapshot");
    assert_eq!(dest_list[0].source.identity(), IDENTITY_ONE);
    assert_eq!(
        src_times_1(&dest_list),
        src_times_1(&source_list),
        "migrate must preserve the source startTime"
    );

    // (f) Idempotency: a second run copies nothing new (keyed on
    // (source, startTime) — dest-side previous manifests).
    dest1
        .snapshot_migrate(&migrate_all)
        .await
        .expect("second snapshot migrate run");
    let dest_list = dest1
        .snapshot_list_all()
        .await
        .expect("dest1 list after rerun");
    assert_eq!(
        dest_list.len(),
        1,
        "re-running migrate must not duplicate the snapshot"
    );

    // Grow the source: a second snapshot of IDENTITY_ONE (changed content, so
    // a later startTime) and a first snapshot of IDENTITY_TWO.
    std::fs::write(data_dir.path().join("a.txt"), b"migrate me again\n").unwrap();
    src_admin
        .snapshot_create(
            data_dir.path().to_str().unwrap(),
            &BTreeMap::new(),
            Some(IDENTITY_ONE),
        )
        .await
        .expect("source snapshot create #2");
    std::fs::write(data2_dir.path().join("b.txt"), b"other identity\n").unwrap();
    src_admin
        .snapshot_create(
            data2_dir.path().to_str().unwrap(),
            &BTreeMap::new(),
            Some(IDENTITY_TWO),
        )
        .await
        .expect("source snapshot create for the second identity");

    // (g) `--sources` selectivity: a fresh destination given only IDENTITY_ONE
    // receives BOTH of its snapshots and NOTHING of IDENTITY_TWO. `Copy`
    // exercises the bare `--policies` argv against the real binary.
    let dest2 = create_dest_repo(dest2_repo.path(), dest2_cfg.path()).await;
    dest2
        .snapshot_migrate(&SnapshotMigrateOptions {
            source_config_path: src_ro_config.clone(),
            sources: MigrateSources::List(vec![IDENTITY_ONE.to_string()]),
            latest_only: false,
            parallel: None,
            policies: MigratePolicies::Copy,
        })
        .await
        .expect("snapshot migrate --sources <identity-one> --policies");
    let dest2_list = dest2.snapshot_list_all().await.expect("dest2 list --all");
    assert_eq!(
        dest2_list.len(),
        2,
        "both IDENTITY_ONE snapshots must arrive: {dest2_list:?}"
    );
    assert!(
        dest2_list
            .iter()
            .all(|e| e.source.identity() == IDENTITY_ONE),
        "IDENTITY_TWO must NOT be migrated: {dest2_list:?}"
    );
    let source_all = src_admin
        .snapshot_list_all()
        .await
        .expect("source list --all");
    assert_eq!(
        source_all.len(),
        3,
        "source now holds two IDENTITY_ONE snapshots and one IDENTITY_TWO"
    );
    assert_eq!(src_times_1(&dest2_list), src_times_1(&source_all));

    // (h) `--latest-only`: only the newest IDENTITY_ONE snapshot arrives.
    // `CopyOverwrite` exercises `--policies --overwrite-policies`.
    let dest3 = create_dest_repo(dest3_repo.path(), dest3_cfg.path()).await;
    dest3
        .snapshot_migrate(&SnapshotMigrateOptions {
            source_config_path: src_ro_config.clone(),
            sources: MigrateSources::List(vec![IDENTITY_ONE.to_string()]),
            latest_only: true,
            parallel: None,
            policies: MigratePolicies::CopyOverwrite,
        })
        .await
        .expect("snapshot migrate --latest-only --policies --overwrite-policies");
    let dest3_list = dest3.snapshot_list_all().await.expect("dest3 list --all");
    assert_eq!(
        dest3_list.len(),
        1,
        "--latest-only must copy exactly one snapshot: {dest3_list:?}"
    );
    assert_eq!(
        dest3_list[0].start_time,
        *src_times_1(&source_all)
            .last()
            .expect("source has snapshots"),
        "--latest-only must pick the NEWEST source snapshot"
    );

    // Cache-pin hazard (the gotcha the mover flow must design around): a
    // migrating client that pins `KOPIA_CACHE_DIRECTORY` — the standard env
    // every OTHER kopiur client sets — shares ONE cache directory across every
    // repository the process opens, so migrate's source open reads the
    // DESTINATION's cached format blob and fails with "invalid repository
    // password" even though every password is correct.
    let poisoned = isolated_client(dest4_cfg.path(), DEST_PASSWORD);
    poisoned
        .repository_create(
            &ConnectSpec::Filesystem {
                path: dest4_repo.path().to_path_buf(),
            },
            Default::default(),
            &Default::default(),
        )
        .await
        .expect("dest4 repository create");
    let err = poisoned
        .snapshot_migrate(&SnapshotMigrateOptions {
            source_config_path: src_ro_config.clone(),
            sources: MigrateSources::All,
            latest_only: false,
            parallel: None,
            policies: MigratePolicies::None,
        })
        .await
        .expect_err(
            "migrate from a KOPIA_CACHE_DIRECTORY-pinned client must fail: the shared \
             cache poisons the source open with the destination's format blob",
        );
    let msg = err.to_string();
    assert!(
        msg.contains("source repository"),
        "the failure must be the SOURCE open, got: {msg}"
    );
}
