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
//! * **`repository throttle set` does NOT need a read-write window.** It succeeds
//!   directly on a `--readonly` connection — contradicting the design premise
//!   this milestone was written to confirm (kopia registers it as a write action,
//!   but it never puts a blob, so read-only storage never rejects it). The
//!   sequence that shipped is therefore `connect --readonly` → `throttle set` →
//!   migrate, with **no flip** — that is what steps (1b)→(6) assert, end to end,
//!   and it is the sequence the timing probe measures. `repository set-client
//!   --read-only/--read-write` consequently stays UNWIRED in the mover flows;
//!   the flip coverage below is a contingency guard, kept so that a future kopia
//!   turning `throttle set` into a real write action finds the alternative
//!   already proven rather than having to discover it under an outage.
//! * **…and that sequence leaves the source READ-ONLY.** `throttle set` rewrites
//!   the config file, so "the bit survives the rewrite" is a data-safety
//!   invariant for a replication source, not a formality. Step (1c) asserts it
//!   both in the config JSON and behaviorally, immediately after the cap lands
//!   and before anything flips.
//! * `repository set-client --read-only` / `--read-write` works on a config
//!   connected `--readonly`, and only rewrites the LOCAL config's `readonly` key
//!   (`true` present / key absent when read-write) — the backend is fingerprinted
//!   across the flip to pin that nothing reaches it.
//! * The flip is real, not cosmetic: `repository set-parameters` — a genuinely
//!   write-requiring verb, and the one that DOES need the window — fails while
//!   read-only, succeeds inside the flip window, and fails again after the flip
//!   back.
//! * Limits persist as `throttlingLimits` in the config JSON and survive the flip
//!   sequence untouched.
//! * The load-bearing claim for the source side: `snapshot migrate
//!   --source-config <throttled config>` genuinely honors those config-persisted
//!   limits. `snapshot migrate` has no speed flags at all, so the persisted
//!   config is the only lever there is — if this were false, the whole
//!   throttle-the-source design would be inert. The bound is self-calibrating:
//!   the test times its own UNTHROTTLED baseline migrate first and requires the
//!   capped one to be at least an order of magnitude slower, so the verdict does
//!   not depend on how fast the machine is.
//! * The caveat that comes with it: throttling only bites on traffic that
//!   actually reaches the backend. A warm source cache makes even a 1 KB/s cap
//!   invisible (the migrate serves from local disk and finishes in ~200 ms), so
//!   both timed migrates drop the cache first — which is also what a fresh mover
//!   pod looks like.

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

/// How many times slower than the UNTHROTTLED baseline the capped migrate must
/// be. The bound is relative on purpose: the test measures its own baseline on
/// the machine it is running on (an identical migrate of the identical fixture,
/// cold cache, no cap) instead of hard-coding a wall-clock figure that a slow CI
/// box could breach for the wrong reason. The measured ratio here is ~68x
/// (14.2 s vs 0.21 s), so 10x leaves ~7x of headroom, while a completely
/// ignored throttle lands at ~1x and cannot squeak past.
const MIGRATE_MIN_SLOWDOWN_FACTOR: u32 = 10;

/// A wall-clock floor kept ALONGSIDE the relative bound, as a backstop for the
/// degenerate case where the baseline itself is pathologically slow (a stalled
/// CI runner) and would make the ratio trivially satisfiable. Well under the
/// ~14 s measured at [`MIGRATE_CAP_BYTES_PER_SEC`]; throttling can only ever
/// make a run slower, so a faster machine cannot push a genuinely throttled run
/// below it.
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

/// Assert the connection is read-only, BOTH ways: the recorded bit in the config
/// JSON, and behaviorally — a write-requiring verb still gets rejected. Asserting
/// only the first would let a kopia that records the bit but stops honoring it
/// pass; asserting only the second would miss the config regressing under a later
/// rewrite. `probe_count` must differ from the last value successfully written,
/// so a rejection can never be kopia short-circuiting a no-change write.
async fn assert_read_only(client: &KopiaClient, config_dir: &std::path::Path, probe_count: i64) {
    assert_eq!(
        read_config(config_dir).get("readonly"),
        Some(&serde_json::Value::Bool(true)),
        "the config must record `readonly: true`"
    );
    let err = try_set_epoch_advance_on_count(client, probe_count)
        .await
        .expect_err("a write-requiring verb must be rejected while read-only");
    assert!(
        err.to_string().contains("read-only") || err.to_string().contains("readonly"),
        "expected a read-only storage rejection, got: {err}"
    );
}

/// Every file in the repository backend, by path and size. Used to pin the claim
/// that `set-client` touches nothing on the backend — a claim that would
/// otherwise be inference from "it works on read-only storage".
fn backend_fingerprint(repo_dir: &std::path::Path) -> BTreeMap<std::path::PathBuf, u64> {
    fn walk(dir: &std::path::Path, out: &mut BTreeMap<std::path::PathBuf, u64>) {
        for entry in std::fs::read_dir(dir).expect("read repository directory") {
            let entry = entry.expect("repository directory entry");
            let meta = entry.metadata().expect("repository entry metadata");
            if meta.is_dir() {
                walk(&entry.path(), out);
            } else {
                out.insert(entry.path(), meta.len());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(repo_dir, &mut out);
    out
}

/// Run one `snapshot migrate` of the whole source into `dest` and return how long
/// it took, with the source cache dropped first.
///
/// Dropping the cache is load-bearing, not hygiene: kopia only throttles traffic
/// that actually reaches the backend, and the migrating process reads the source
/// through the cache directory recorded in the SOURCE config
/// (`caching.cacheDirectory`, relative to the config file → `<cfg>/cache`). Left
/// warm, the migrate serves everything from local disk and finishes unthrottled
/// in ~200 ms whatever the cap says. A fresh mover pod's cache is cold by
/// construction, which is what this models — and it is also the caveat the
/// dependent milestones have to design around.
async fn timed_cold_migrate(
    dest: &KopiaClient,
    src_ro_cfg: &std::path::Path,
) -> std::time::Duration {
    std::fs::remove_dir_all(src_ro_cfg.join("cache")).expect("drop the source cache");
    let started = Instant::now();
    dest.snapshot_migrate(&SnapshotMigrateOptions {
        source_config_path: config_path(src_ro_cfg),
        sources: MigrateSources::All,
        latest_only: false,
        parallel: None,
        policies: MigratePolicies::None,
    })
    .await
    .expect("snapshot migrate");
    started.elapsed()
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
    let base_repo = tempfile::tempdir().unwrap();
    let base_cfg = tempfile::tempdir().unwrap();

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

    // Two destinations: one for the unthrottled BASELINE migrate that calibrates
    // the timing bound, one for the throttled measurement. They must be separate
    // repositories — migrate is idempotent by (source, startTime), so a second
    // run into the same destination would copy nothing and time nothing.
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
    let baseline_dest = dest_client(base_cfg.path(), DEST_PASSWORD);
    baseline_dest
        .repository_create(
            &ConnectSpec::Filesystem {
                path: base_repo.path().to_path_buf(),
            },
            Default::default(),
            &Default::default(),
        )
        .await
        .expect("baseline destination repository create");

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
    assert_read_only(&src_ro, src_ro_cfg.path(), 10).await;

    // (1a) BASELINE: an identical migrate with NO cap, on this machine, to
    // calibrate the timing bound in step (6). Taken before any throttle exists so
    // it measures the genuinely unthrottled cost.
    let baseline = timed_cold_migrate(&baseline_dest, src_ro_cfg.path()).await;

    // (1b) THE PRODUCTION SEQUENCE, and the premise this milestone was written to
    // check — which does NOT hold as the plan stated it. `repository throttle set`
    // SUCCEEDS on the read-only connection: it only rewrites the local config's
    // `throttlingLimits`, it never puts a blob. So what shipped is `connect
    // --readonly` → `throttle set` → migrate, with NO flip, and that is the
    // sequence measured in step (6).
    //
    // If a future kopia turns this into a real write action, this fails loudly and
    // the flip below stops being defensive and becomes mandatory.
    src_ro
        .repository_throttle_set(&ThrottleArgs {
            upload_bytes_per_second: Some(MIGRATE_CAP_BYTES_PER_SEC),
            download_bytes_per_second: Some(MIGRATE_CAP_BYTES_PER_SEC),
            ..Default::default()
        })
        .await
        .expect("throttle set succeeds on a READ-ONLY connection (kopia 0.23.1)");
    let limits = read_config(src_ro_cfg.path())["throttlingLimits"].clone();
    assert_eq!(
        limits["maxUploadSpeedBytesPerSecond"],
        serde_json::json!(MIGRATE_CAP_BYTES_PER_SEC),
        "throttle set must persist into the local config even when read-only: {limits}"
    );
    assert_eq!(
        limits["maxDownloadSpeedBytesPerSecond"],
        serde_json::json!(MIGRATE_CAP_BYTES_PER_SEC),
        "throttle set must persist into the local config even when read-only: {limits}"
    );

    // (1c) …AND THE SOURCE IS STILL READ-ONLY. This is the data-safety invariant
    // of the whole no-flip sequence: `throttle set` rewrites the config file, and
    // a rewrite that dropped the `readonly` key would silently leave a replication
    // SOURCE writable. Asserted both ways — the recorded bit and the behavioral
    // probe — because nothing downstream would notice otherwise: step (2) flips
    // read-write on purpose, and step (3) re-establishes the bit explicitly.
    assert_read_only(&src_ro, src_ro_cfg.path(), 11).await;

    // (2) The flip window itself — kept because the client method has to be
    // proven for the verbs that genuinely DO need it. Opening it makes the same
    // write-requiring verb that failed twice above SUCCEED.
    let backend_before = backend_fingerprint(src_repo.path());
    src_ro
        .repository_set_client_read_only(false)
        .await
        .expect("set-client --read-write");
    assert!(
        read_config(src_ro_cfg.path()).get("readonly").is_none(),
        "read-write must clear the config's `readonly` key"
    );
    assert_eq!(
        backend_fingerprint(src_repo.path()),
        backend_before,
        "set-client must not touch the backend — the flip is a LOCAL config edit"
    );
    try_set_epoch_advance_on_count(&src_ro, 10)
        .await
        .expect("set-parameters must succeed inside the read-write flip window");

    // (3) Flip back, and (4) assert read-only is genuinely restored — with a
    // DIFFERENT epoch count (12, not the 10 just committed) so the rejection
    // cannot be a no-change short-circuit.
    src_ro
        .repository_set_client_read_only(true)
        .await
        .expect("set-client --read-only");
    assert_read_only(&src_ro, src_ro_cfg.path(), 12).await;

    // (5) The cap set back at (1b), on the read-only connection, survived the
    // whole flip sequence untouched — so the config step (6) measures is the one
    // the no-flip production sequence wrote.
    let limits = read_config(src_ro_cfg.path())["throttlingLimits"].clone();
    assert_eq!(
        limits["maxUploadSpeedBytesPerSecond"],
        serde_json::json!(MIGRATE_CAP_BYTES_PER_SEC),
        "upload cap must survive the flip sequence: {limits}"
    );
    assert_eq!(
        limits["maxDownloadSpeedBytesPerSecond"],
        serde_json::json!(MIGRATE_CAP_BYTES_PER_SEC),
        "download cap must survive the flip sequence: {limits}"
    );

    // (6) THE LOAD-BEARING CLAIM: `snapshot migrate` reopens the source config
    // and honors its persisted limits. `snapshot migrate` exposes no speed
    // flags, so if this were false there would be no way to throttle a
    // seed-migrate at all.
    //
    // The config this reads is the one the NO-FLIP sequence wrote at (1b) and
    // (5) proved untouched since — i.e. this measures exactly what production
    // will do, not a flip-bracketed variant of it.
    let elapsed = timed_cold_migrate(&dest, src_ro_cfg.path()).await;

    let migrated = dest.snapshot_list_all().await.expect("dest list --all");
    assert_eq!(
        migrated.len(),
        1,
        "the snapshot must actually arrive: {migrated:?}"
    );
    assert_eq!(migrated[0].source.identity(), IDENTITY);

    // Both bounds, and the failure message carries the baseline so a breach is
    // diagnosable rather than just "too fast". The mode this guards is "the
    // throttle was ignored entirely", which lands at ~1x the baseline.
    assert!(
        elapsed >= baseline * MIGRATE_MIN_SLOWDOWN_FACTOR,
        "migrate finished in {elapsed:?} with the source config capped at \
         {MIGRATE_CAP_BYTES_PER_SEC} B/s, against an UNTHROTTLED baseline of \
         {baseline:?} for the identical migrate — under {MIGRATE_MIN_SLOWDOWN_FACTOR}x \
         it must have IGNORED the persisted throttle. The entire source-side \
         throttle design rests on migrate honoring it."
    );
    assert!(
        elapsed.as_secs() >= MIGRATE_MIN_ELAPSED_SECS,
        "migrate finished in {elapsed:?} with the source config capped at \
         {MIGRATE_CAP_BYTES_PER_SEC} B/s (baseline {baseline:?}) — below the \
         {MIGRATE_MIN_ELAPSED_SECS}s absolute floor, so the relative bound was \
         satisfied only because the baseline itself was pathologically slow."
    );
}
