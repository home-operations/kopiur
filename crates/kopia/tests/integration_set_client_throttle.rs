//! Real-kopia integration test for the read-only/read-write client flip and the
//! throttle limits it brackets (issue #374, milestone M3a).
//!
//! Gated behind the `integration` feature and `#[ignore]` by default so the
//! hermetic `cargo test` never invokes the real binary (the `integration`
//! feature lifts the `#[ignore]`, so it is not needed on the command line):
//!
//! ```text
//! cargo test -p kopiur-kopia --features integration --test integration_set_client_throttle
//! ```
//!
//! The kopia binary comes from mise's PATH — the SAME pin (`v0.23.1`) the mover
//! image ships — so a Renovate bump moves both and this test fails loudly on
//! semantic drift. That is the point: everything asserted here is a premise the
//! replication/seed movers' throttle handling rests on, and none of it is
//! documented by kopia.
//!
//! What it pins against kopia 0.23.1:
//!
//! * `repository set-client --read-only` / `--read-write` works on a config
//!   connected `--readonly`, and only rewrites the LOCAL config's `readonly` key
//!   (`true` present / key absent when read-write). Nothing reaches the backend.
//! * The flip is real, not cosmetic: `repository set-parameters` — a genuinely
//!   write-requiring verb — fails while read-only, succeeds inside the flip
//!   window, and fails again after the flip back.
//! * **`repository throttle set` does NOT need the flip window.** It succeeds
//!   directly on a `--readonly` connection — contradicting the design premise
//!   this milestone was written to confirm (kopia registers it as a write action,
//!   but it never puts a blob, so read-only storage never rejects it). The flip
//!   is therefore defensive for throttling, not required. If a future kopia turns
//!   it into a real write, step (1b) below fails loudly and the flip becomes
//!   mandatory.
//! * Limits persist as `throttlingLimits` in the config JSON and survive the flip
//!   back to read-only.
//! * The load-bearing claim for the source side: `snapshot migrate
//!   --source-config <throttled config>` genuinely honors those config-persisted
//!   limits. `snapshot migrate` has no speed flags at all, so the persisted
//!   config is the only lever there is — if this were false, the whole
//!   throttle-the-source design would be inert.
//! * The caveat that comes with it: throttling only bites on traffic that
//!   actually reaches the backend. A warm source cache makes even a 1 KB/s cap
//!   invisible (the migrate serves from local disk and finishes in ~200 ms), so
//!   step (6) drops the cache first — which is also what a fresh mover pod looks
//!   like.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::time::Instant;

use kopiur_kopia::client::SetParametersArgs;
use kopiur_kopia::{
    ConnectOptions, ConnectSpec, KopiaClient, MigratePolicies, MigrateSources,
    SnapshotMigrateOptions, ThrottleArgs,
};

const SRC_PASSWORD: &str = "setclient-pass-a";
const DEST_PASSWORD: &str = "setclient-pass-b";
const IDENTITY: &str = "throttleuser@throttlehost:/data";

/// The cap used for the migrate timing probe, in bytes/sec.
///
/// Chosen from a measured curve, not guessed. Against this fixture (an 8 KiB
/// incompressible payload → a ~28 KiB source repository) on the pinned binary,
/// with a COLD source cache, `snapshot migrate` takes:
///
/// | cap B/s   | elapsed |
/// |-----------|---------|
/// | unlimited | 0.21 s  |
/// | 20 M      | 0.21 s  |
/// | 10 M      | 0.21 s  |
/// |  5 M      | 2.2 s   |
/// |  2 M      | 14.2 s  |
/// |  1 M      | 34.2 s  |
///
/// Note how brutally non-linear that is: kopia's throttler costs far more than
/// the nominal rate implies (28 KiB "should" take 14 ms at 2 MB/s, and takes
/// 14 s), and caps at or above ~10 MB/s do not bind at all here. 2 MB/s is the
/// sweet spot — a ~14 s run, ~5x above the assertion floor and ~70x above the
/// unthrottled time, while a 1 MB/s cap would drag the test to 35 s and lower
/// caps to hours.
const MIGRATE_CAP_BYTES_PER_SEC: i64 = 2_000_000;

/// The wall-clock floor a throttled migrate must clear, well under the ~14 s
/// measured at [`MIGRATE_CAP_BYTES_PER_SEC`] so a loaded CI box cannot make it
/// flake. The failure it guards is "the throttle was ignored entirely", which
/// lands at ~0.2 s — two orders of magnitude below this floor, so the verdict is
/// never ambiguous. Throttling can only ever make a run SLOWER, so a faster
/// machine cannot push a genuinely throttled run below the bound.
const MIGRATE_MIN_ELAPSED_SECS: u64 = 3;

fn config_path(config_dir: &std::path::Path) -> String {
    config_dir.join("repository.config").display().to_string()
}

/// A client whose kopia state is isolated inside `config_dir`, same pattern as
/// `integration_migrate.rs`.
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

/// A DESTINATION client able to run `snapshot migrate`: it must NOT pin
/// `KOPIA_CACHE_DIRECTORY`, or migrate's source open reads the destination's
/// cached format blob and fails with "invalid repository password" (proven by
/// `integration_migrate.rs`). Cache isolation comes from `XDG_CACHE_HOME`.
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

/// The parsed kopia config JSON for a client's config directory. The read-only
/// bit and the throttling limits both live here — this test asserts on the file
/// because that persistence IS the mechanism the movers depend on.
fn read_config(config_dir: &std::path::Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(config_path(config_dir)).expect("read kopia config JSON");
    serde_json::from_str(&raw).expect("kopia config is JSON")
}

/// A genuinely write-requiring verb, used to prove the read-only bit is really
/// in force (not just recorded). `repository set-parameters` must reach the
/// backend to rewrite `kopia.blobcfg`, so a read-only connection hard-errors.
/// The value varies per call so a rejected attempt can never be confused with
/// kopia skipping a no-change write.
async fn try_set_epoch_advance_on_count(
    client: &KopiaClient,
    count: i64,
) -> Result<(), kopiur_kopia::KopiaError> {
    client
        .repository_set_parameters(&SetParametersArgs {
            epoch_advance_on_count: Some(count),
            ..Default::default()
        })
        .await
}

#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn set_client_flip_brackets_throttle_and_migrate_honors_persisted_limits() {
    let src_repo = tempfile::tempdir().unwrap();
    let src_admin_cfg = tempfile::tempdir().unwrap();
    let src_ro_cfg = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let dest_repo = tempfile::tempdir().unwrap();
    let dest_cfg = tempfile::tempdir().unwrap();

    // ---- fixture: a source repository holding one snapshot of 8 KiB of
    // deliberately incompressible-but-DETERMINISTIC bytes (a Knuth-multiplicative
    // scramble). Deterministic keeps the calibrated timings in
    // `MIGRATE_CAP_BYTES_PER_SEC` reproducible run to run; incompressible stops a
    // future compression default from shrinking the payload and quietly robbing
    // the timing probe of the bytes it measures.
    let payload: Vec<u8> = (0..8192u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    std::fs::write(data_dir.path().join("blob.bin"), &payload).unwrap();
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
    src_admin
        .snapshot_create(
            data_dir.path().to_str().unwrap(),
            &BTreeMap::new(),
            Some(IDENTITY),
        )
        .await
        .expect("source snapshot create");

    let dest = dest_client(dest_cfg.path(), DEST_PASSWORD);
    dest.repository_create(
        &ConnectSpec::Filesystem {
            path: dest_repo.path().to_path_buf(),
        },
        Default::default(),
        &Default::default(),
    )
    .await
    .expect("destination repository create");

    // ---- the connect the replication/seed movers perform for their SOURCE:
    // a separate config, read-only, credentials persisted beside it.
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
    assert_eq!(
        read_config(src_ro_cfg.path()).get("readonly"),
        Some(&serde_json::Value::Bool(true)),
        "a --readonly connect must record `readonly: true` in the config"
    );

    // (1) The read-only bit is REALLY in force: a verb that must write a blob
    // fails. This is the control for everything below — without it, "throttle
    // set works on read-only" would prove nothing.
    let err = try_set_epoch_advance_on_count(&src_ro, 10)
        .await
        .expect_err("set-parameters must fail on a read-only connection");
    assert!(
        err.to_string().contains("read-only") || err.to_string().contains("readonly"),
        "expected a read-only storage rejection, got: {err}"
    );

    // (1b) THE PREMISE THIS MILESTONE WAS WRITTEN TO CHECK — and it does not
    // hold. `repository throttle set` SUCCEEDS on the read-only connection: it
    // only rewrites the local config's `throttlingLimits`, it never puts a blob.
    // The flip window is therefore DEFENSIVE for throttling, not required.
    // If a future kopia turns this into a real write action, this line fails
    // loudly and the dependent milestones must adopt the flip unconditionally.
    let benign = ThrottleArgs {
        upload_bytes_per_second: Some(1_000_000),
        download_bytes_per_second: Some(1_000_000),
        ..Default::default()
    };
    src_ro
        .repository_throttle_set(&benign)
        .await
        .expect("throttle set succeeds on a READ-ONLY connection (kopia 0.23.1)");
    assert_eq!(
        read_config(src_ro_cfg.path())["throttlingLimits"]["maxUploadSpeedBytesPerSecond"],
        serde_json::json!(1_000_000),
        "throttle set must persist into the local config even when read-only"
    );

    // (2) Open the flip window and prove it is real: the same write-requiring
    // verb that failed above now SUCCEEDS.
    src_ro
        .repository_set_client_read_only(false)
        .await
        .expect("set-client --read-write");
    assert!(
        read_config(src_ro_cfg.path()).get("readonly").is_none(),
        "read-write must clear the config's `readonly` key"
    );
    try_set_epoch_advance_on_count(&src_ro, 10)
        .await
        .expect("set-parameters must succeed inside the read-write flip window");

    // Apply the cap the timing probe measures, inside the window — this is the
    // exact flip → throttle set → flip-back sequence the dependent milestones use.
    src_ro
        .repository_throttle_set(&ThrottleArgs {
            upload_bytes_per_second: Some(MIGRATE_CAP_BYTES_PER_SEC),
            download_bytes_per_second: Some(MIGRATE_CAP_BYTES_PER_SEC),
            ..Default::default()
        })
        .await
        .expect("throttle set inside the flip window");

    // (3) Flip back, and (4) assert read-only is genuinely restored — config bit
    // AND the behavioral probe, with a DIFFERENT epoch count (12, not the 10 just
    // committed) so the rejection cannot be a no-change short-circuit.
    src_ro
        .repository_set_client_read_only(true)
        .await
        .expect("set-client --read-only");
    assert_eq!(
        read_config(src_ro_cfg.path()).get("readonly"),
        Some(&serde_json::Value::Bool(true)),
        "the flip back must restore `readonly: true`"
    );
    let err = try_set_epoch_advance_on_count(&src_ro, 12)
        .await
        .expect_err("set-parameters must fail again after the flip back");
    assert!(
        err.to_string().contains("read-only") || err.to_string().contains("readonly"),
        "expected a read-only storage rejection after the flip back, got: {err}"
    );

    // (5) The limits survived the whole flip sequence, in the config on disk.
    let limits = read_config(src_ro_cfg.path())["throttlingLimits"].clone();
    assert_eq!(
        limits["maxUploadSpeedBytesPerSecond"],
        serde_json::json!(MIGRATE_CAP_BYTES_PER_SEC),
        "upload cap must survive the flip back: {limits}"
    );
    assert_eq!(
        limits["maxDownloadSpeedBytesPerSecond"],
        serde_json::json!(MIGRATE_CAP_BYTES_PER_SEC),
        "download cap must survive the flip back: {limits}"
    );

    // (6) THE LOAD-BEARING CLAIM: `snapshot migrate` reopens the source config
    // and honors its persisted limits. `snapshot migrate` exposes no speed
    // flags, so if this were false there would be no way to throttle a
    // seed-migrate at all.
    //
    // The source cache must be COLD first. kopia only throttles traffic that
    // actually reaches the backend, and the migrating process reads the source
    // through the cache directory recorded in the SOURCE config
    // (`caching.cacheDirectory`, relative to the config file → `<cfg>/cache`).
    // The earlier steps warmed it; leaving it warm makes the migrate serve
    // everything from local disk and finish unthrottled in ~200ms — which is
    // also the honest caveat for the dependent milestones: throttling only
    // bites on real backend traffic, and a mover pod's cache is cold by
    // construction, which is what this models.
    std::fs::remove_dir_all(src_ro_cfg.path().join("cache")).expect("drop the source cache");

    let started = Instant::now();
    dest.snapshot_migrate(&SnapshotMigrateOptions {
        source_config_path: config_path(src_ro_cfg.path()),
        sources: MigrateSources::All,
        latest_only: false,
        parallel: None,
        policies: MigratePolicies::None,
    })
    .await
    .expect("throttled snapshot migrate");
    let elapsed = started.elapsed();

    let migrated = dest.snapshot_list_all().await.expect("dest list --all");
    assert_eq!(
        migrated.len(),
        1,
        "the snapshot must actually arrive: {migrated:?}"
    );
    assert_eq!(migrated[0].source.identity(), IDENTITY);
    assert!(
        elapsed.as_secs() >= MIGRATE_MIN_ELAPSED_SECS,
        "migrate finished in {elapsed:?} with the source config capped at \
         {MIGRATE_CAP_BYTES_PER_SEC} B/s — it must have IGNORED the persisted \
         throttle (an unthrottled migrate of this fixture is sub-second). The \
         entire source-side throttle design rests on migrate honoring it."
    );
}
