//! Real-kopia integration test for the epoch-parameter floors the admission webhook
//! enforces (issue #458).
//!
//! Gated behind the `integration` feature and `#[ignore]` by default so the hermetic
//! `cargo test` never invokes the real binary (the `integration` feature lifts the
//! `#[ignore]`, so it is not needed on the command line):
//!
//! ```text
//! cargo test -p kopiur-kopia --features integration --test integration_epoch_floors
//! ```
//!
//! The kopia binary comes from mise's PATH — the SAME pin (`v0.23.1`) the mover image
//! ships — so a Renovate bump moves both and this test fails loudly on semantic drift.
//! That is the whole point: the epoch floors behind
//! `kopiur_api::validate::validate_repository_parameters` (the private `epoch_floor_errors`
//! in `crates/api/src/validate/repository.rs`) QUOTE kopia's error strings back to the user,
//! and a quote nobody checks is a lie waiting to happen.
//!
//! What it pins against kopia 0.23.1:
//!
//! * **Every floor the webhook enforces is genuinely kopia's**, with the exact error text
//!   the admission message quotes: `advance-on-count` < 10 → "epoch advance on count too
//!   low"; `advance-on-size-mb` < 1 → "epoch advance on size too low";
//!   `checkpoint-frequency` < 1 → "invalid epoch range compaction period";
//!   `min-duration` < 10m → "minimum epoch duration too low"; `refresh-frequency` whose
//!   3x exceeds the cleanup safety margin → "invalid cleanup safety margin, must be at
//!   least 3x epoch refresh frequency".
//! * **`set-parameters` MERGES, then validates the whole set** — the finding that makes
//!   #458 a data problem rather than a cosmetic one. Sending one out-of-range value
//!   together with a perfectly legal one refuses the ENTIRE call and the legal one is
//!   discarded with it. That is exactly what happened to the reporter: they declared
//!   `advanceOnCount: 5` + `advanceOnSizeMiB: 2` next to a legal `minDuration`, and the
//!   `minDuration` never landed either.
//! * **`minDuration: 10m` declared ALONE always fails**, even though 10m is exactly
//!   kopia's absolute floor: the repository's untouched 20m refresh makes 20m x 3 = 60m
//!   the effective bound. Declaring a small enough `refreshFrequency` alongside it makes
//!   the identical 10m succeed. This is why the webhook's floor is
//!   `max(10m, 3 x refreshFrequency-or-kopia's-20m-default)` and why its fix text offers
//!   both branches.
//! * **kopia's CLI silently DROPS a `0`** on these integer flags: it reports `no changes`
//!   and the repository keeps its old value. A `0` is therefore the worst possible
//!   outcome — it looks like it applied — which is why the webhook rejects it rather than
//!   treating it as "leave this alone".
//! * **The reporter's values, corrected, actually apply and are visible** in
//!   `repository status --json`.
//! * **A zero DURATION is dropped exactly like a zero integer**, which is the hole the first
//!   pass at #458 left open: `refreshFrequency` had a ceiling but no floor, so `0s` was
//!   admitted, silently applied nothing, and would then read as permanent drift on every
//!   bootstrap.
//! * **The 80m ceiling's assumption is an assumption.** kopia's CLI exposes
//!   `--epoch-cleanup-safety-margin` (kopiur has no field for it), and with the margin
//!   raised to 23h a 90m refresh is genuinely accepted — so the ceiling can reject a call
//!   kopia would allow. The admission message says so and points at the mirrored live value
//!   rather than claiming the 4h default as fact.
//! * **`maintenance set --owner` does not rewrite `schedule.runs`.** Load-bearing for the
//!   before/after run-delta work that builds on this: if setting the owner also rewrote
//!   the run history, a delta taken across an owner restamp would be meaningless.
//!   Asserted BYTE-exactly on kopia's own emitted JSON, and for both the `--owner me`
//!   no-op and a genuine owner change.

#![cfg(unix)]

use kopiur_kopia::client::SetParametersArgs;
use kopiur_kopia::{ConnectSpec, KopiaClient, MaintenanceMode};

const PASSWORD: &str = "epoch-floors-pass";
const MIB: i64 = 1_048_576;

/// kopia's shipped defaults for the parameters this test touches. The floors must be
/// stated against these, because `set-parameters` validates the MERGE of what you send
/// with what the repository already has — so "the untouched value" is a premise of every
/// single-flag call below.
const KOPIA_DEFAULT_ADVANCE_ON_COUNT: i64 = 20;
const KOPIA_DEFAULT_CHECKPOINT_FREQUENCY: i64 = 7;

fn config_path(config_dir: &std::path::Path) -> String {
    config_dir.join("repository.config").display().to_string()
}

/// A client whose kopia state is isolated inside `config_dir`, same pattern as
/// `integration_set_client_throttle.rs`.
fn isolated_client(config_dir: &std::path::Path) -> KopiaClient {
    KopiaClient::builder()
        .binary("kopia")
        .env("KOPIA_PASSWORD", PASSWORD)
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

/// A fresh filesystem repository with kopia's default parameters, plus its client.
async fn fresh_repo() -> (KopiaClient, tempfile::TempDir, tempfile::TempDir) {
    let repo = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let client = isolated_client(cfg.path());
    client
        .repository_create(
            &ConnectSpec::Filesystem {
                path: repo.path().to_path_buf(),
            },
            Default::default(),
            &Default::default(),
        )
        .await
        .expect("repository create");
    (client, repo, cfg)
}

/// The epoch parameters the repository ACTUALLY reports. Every assertion about what did or
/// did not apply reads through here rather than trusting the CLI's exit code.
async fn observed(client: &KopiaClient) -> kopiur_kopia::model::EpochParameters {
    client
        .repository_status()
        .await
        .expect("repository status")
        .content_format
        .epoch_parameters
        .expect("a v3-format repository reports contentFormat.epochParameters")
}

#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn kopia_rejects_every_out_of_range_epoch_parameter_and_refuses_the_whole_call() {
    let (client, _repo, cfg) = fresh_repo().await;

    // (0) The baseline the merge semantics are measured against.
    let base = observed(&client).await;
    assert_eq!(base.advance_on_count, KOPIA_DEFAULT_ADVANCE_ON_COUNT);
    assert_eq!(
        base.checkpoint_frequency,
        KOPIA_DEFAULT_CHECKPOINT_FREQUENCY
    );
    assert_eq!(base.advance_on_total_size_bytes, 10 * MIB, "10 MiB default");

    // (1) THE REPORTER'S VALUE. `advanceOnCount: 5` — the webhook now rejects it, and this
    // is why: kopia does too, with the text the admission message quotes verbatim.
    let err = client
        .repository_set_parameters(&SetParametersArgs {
            epoch_advance_on_count: Some(5),
            ..Default::default()
        })
        .await
        .expect_err("advance-on-count 5 must be refused");
    assert!(
        err.to_string().contains("epoch advance on count too low"),
        "kopiur's admission message quotes this string back to the user: {err}"
    );

    // (2) …and every other floor's quoted text, for the STRING-valued flags, which the
    // client can deliver as-is.
    for (label, args, expected) in [
        (
            "min-duration below kopia's absolute 10m floor",
            SetParametersArgs {
                epoch_min_duration: Some("5m".into()),
                // Paired with a tiny refresh so the 3x rule is satisfied and the ABSOLUTE
                // floor is the only thing left to fail on — otherwise this would prove the
                // wrong rule.
                epoch_refresh_frequency: Some("1m".into()),
                ..Default::default()
            },
            "minimum epoch duration too low",
        ),
        (
            "refresh-frequency above cleanupSafetyMargin/3",
            SetParametersArgs {
                epoch_refresh_frequency: Some("90m".into()),
                ..Default::default()
            },
            "invalid cleanup safety margin, must be at least 3x epoch refresh frequency",
        ),
    ] {
        let Err(err) = client.repository_set_parameters(&args).await else {
            panic!("{label} must be refused by kopia");
        };
        let err = err.to_string();
        assert!(
            err.contains(expected),
            "{label}: expected kopia to say {expected:?}, got: {err}"
        );
    }

    // (2a) A NEGATIVE integer never reaches kopia's validator AT ALL through kopiur.
    // `SetParametersArgs::args()` emits `["--flag", "value"]` as two argv entries, and
    // kopia's flag parser reads a leading `-` as the start of another flag:
    //
    //   kopia: error: expected argument for flag '--epoch-advance-on-size-mb'
    //
    // So a negative value in a CR produces THAT, not kopia's range error — an unhelpful
    // message for a value the webhook now rejects up front anyway. Pinned because it is
    // the one place where kopiur's argv rendering, not kopia's validation, decides the
    // user-visible failure.
    for (flag, args) in [
        (
            "--epoch-advance-on-size-mb",
            SetParametersArgs {
                epoch_advance_on_size_mb: Some(-1),
                ..Default::default()
            },
        ),
        (
            "--epoch-checkpoint-frequency",
            SetParametersArgs {
                epoch_checkpoint_frequency: Some(-1),
                ..Default::default()
            },
        ),
    ] {
        let Err(err) = client.repository_set_parameters(&args).await else {
            panic!("{flag} with a negative value must fail");
        };
        let err = err.to_string();
        assert!(
            err.contains(&format!("expected argument for flag '{flag}'")),
            "{flag}: expected kopia's FLAG PARSER to reject the negative argv, got: {err}"
        );
    }

    // (2b) …and kopia's own range errors — the texts the admission messages quote — reached
    // the only way they can be, with `=`-joined argv. This is what keeps those quotes
    // honest: kopiur's own client cannot deliver these values, but kopia's validator is
    // what the message claims is speaking, so the claim is checked against kopia directly.
    for (flag, expected) in [
        (
            "--epoch-advance-on-size-mb=-1",
            "epoch advance on size too low",
        ),
        (
            "--epoch-checkpoint-frequency=-1",
            "invalid epoch range compaction period",
        ),
        (
            "--epoch-advance-on-count=-1",
            "epoch advance on count too low",
        ),
    ] {
        let err = raw_set_parameters_err(cfg.path(), flag);
        assert!(
            err.contains(expected),
            "{flag}: expected kopia to say {expected:?}, got: {err}"
        );
    }

    // (3) `0` IS SILENTLY DROPPED. kopia's CLI treats it as "flag not set", exits 0 with
    // `no changes`, and the repository keeps its old value — which is why the webhook
    // rejects a 0 instead of reading it as "leave this alone". This is the one case where
    // a user would see success and get nothing.
    client
        .repository_set_parameters(&SetParametersArgs {
            epoch_advance_on_count: Some(0),
            ..Default::default()
        })
        .await
        .expect("kopia's CLI accepts a 0 and does nothing at all");
    assert_eq!(
        observed(&client).await.advance_on_count,
        KOPIA_DEFAULT_ADVANCE_ON_COUNT,
        "a 0 must have changed nothing — that silent no-op is what the floor prevents"
    );

    // (4) THE LOAD-BEARING FINDING. One out-of-range value refuses the WHOLE call: kopia
    // merges the flags into the repository's existing parameters and validates the result
    // as a set. `checkpointFrequency: 9` here is perfectly legal on its own, and it is
    // discarded along with the rejected count. This is precisely what cost the #458
    // reporter their legal `minDuration`.
    let err = client
        .repository_set_parameters(&SetParametersArgs {
            epoch_advance_on_count: Some(5),
            epoch_checkpoint_frequency: Some(9),
            ..Default::default()
        })
        .await
        .expect_err("one bad value must refuse the whole call");
    assert!(
        err.to_string().contains("epoch advance on count too low"),
        "{err}"
    );
    assert_eq!(
        observed(&client).await.checkpoint_frequency,
        KOPIA_DEFAULT_CHECKPOINT_FREQUENCY,
        "the LEGAL checkpointFrequency: 9 in the same call must have been discarded too — \
         this is the merge-then-validate behavior the admission messages warn about"
    );

    // (5) `minDuration: 10m` ALONE fails on the 3x rule, because the repository still has
    // kopia's 20m refresh (20m x 3 = 60m > 10m). The webhook's derived floor exists for
    // exactly this, and its fix text says so.
    let err = client
        .repository_set_parameters(&SetParametersArgs {
            epoch_min_duration: Some("10m".into()),
            ..Default::default()
        })
        .await
        .expect_err("10m alone cannot clear 3 x the untouched 20m refresh");
    assert!(
        err.to_string()
            .contains("epoch refresh period is too long, must be 1/3 of minimal epoch"),
        "{err}"
    );

    // (6) …and the IDENTICAL 10m succeeds the moment a small enough refresh rides with it.
    // The webhook's alternative fix ("declare a refreshFrequency of a third or less") has
    // to actually work, or it is worse than no advice.
    client
        .repository_set_parameters(&SetParametersArgs {
            epoch_min_duration: Some("10m".into()),
            epoch_refresh_frequency: Some("3m".into()),
            ..Default::default()
        })
        .await
        .expect("10m + a 3m refresh satisfies both floors");
    let o = observed(&client).await;
    assert_eq!(o.min_epoch_duration_ns, 600_000_000_000, "10m");
    assert_eq!(o.epoch_refresh_frequency_ns, 180_000_000_000, "3m");

    // (7) THE REPORTER'S DECLARATION, CORRECTED: count at kopia's floor of 10 and the 2 MiB
    // they asked for. Both apply, and both are visible in the repository's own status —
    // the end-to-end proof that nothing in the kopiur→kopia path drops these fields, which
    // is what sent #458 looking at kopia's validator in the first place.
    client
        .repository_set_parameters(&SetParametersArgs {
            epoch_advance_on_count: Some(10),
            epoch_advance_on_size_mb: Some(2),
            ..Default::default()
        })
        .await
        .expect("count 10 + size 2 MiB are both inside kopia's ranges");
    let o = observed(&client).await;
    assert_eq!(o.advance_on_count, 10);
    assert_eq!(
        o.advance_on_total_size_bytes,
        2 * MIB,
        "kopia stores the MiB flag as `v << 20`, which is why kopiur's drift check compares \
         in bytes and not in truncated MiB"
    );

    // (8) A ZERO DURATION is dropped exactly like a zero integer: exit 0, `no changes`, and
    // the repository keeps the refresh set back at step (6). This is the hole #458's first
    // pass left open — `refreshFrequency` had a ceiling but no floor — and it is the worst
    // shape of the bug, because kopiur's drift comparator then reads the untouched live
    // value as permanent drift and re-issues the flag on every bootstrap forever. The
    // admission message quotes this behavior, so it is pinned here.
    let before_zero = observed(&client).await.epoch_refresh_frequency_ns;
    client
        .repository_set_parameters(&SetParametersArgs {
            epoch_refresh_frequency: Some("0s".into()),
            ..Default::default()
        })
        .await
        .expect("kopia accepts a zero duration and does nothing at all");
    assert_eq!(
        observed(&client).await.epoch_refresh_frequency_ns,
        before_zero,
        "a zero duration must have changed nothing — the silent no-op the floor prevents"
    );

    // (9) THE CEILING'S ASSUMPTION, stated honestly in the admission message: 80m is 4h/3
    // and kopiur assumes the 4h default because a validator cannot read live state — but
    // kopia's CLI DOES expose `--epoch-cleanup-safety-margin`, so a repository whose margin
    // was raised out of band genuinely accepts a 90m refresh. kopiur has no field for the
    // margin, so this is driven through raw argv. Last, because it mutates the premise every
    // step above rests on.
    let out = std::process::Command::new("kopia")
        .args([
            "repository",
            "set-parameters",
            "--epoch-cleanup-safety-margin=23h",
        ])
        .env("KOPIA_PASSWORD", PASSWORD)
        .env("KOPIA_CONFIG_PATH", config_path(cfg.path()))
        .env("KOPIA_CACHE_DIRECTORY", cfg.path().join("cache"))
        .env("KOPIA_LOG_DIR", cfg.path().join("logs"))
        .env("KOPIA_CHECK_FOR_UPDATES", "false")
        .output()
        .expect("run kopia repository set-parameters --epoch-cleanup-safety-margin");
    assert!(
        out.status.success(),
        "kopia must expose --epoch-cleanup-safety-margin: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // `minDuration` rides along, restored to kopia's 24h default: step (6) left it at 10m,
    // and a 90m refresh also has to clear `minDuration >= 3 x refresh` (270m). Sending both
    // isolates the margin rule — and is itself the merge semantics at work, this time
    // admitting a call that either value alone would fail.
    client
        .repository_set_parameters(&SetParametersArgs {
            epoch_refresh_frequency: Some("90m".into()),
            epoch_min_duration: Some("24h".into()),
            ..Default::default()
        })
        .await
        .expect(
            "with the margin at 23h, a 90m refresh is LEGAL — so kopiur's 80m ceiling can \
             reject a call kopia would accept, which is why its message owns the assumption \
             and points at status.parameters.epoch.cleanupSafetyMargin",
        );
    assert_eq!(
        observed(&client).await.epoch_refresh_frequency_ns,
        5_400_000_000_000,
        "90m"
    );
}

/// Run `kopia repository set-parameters <flag>` with the flag and its value JOINED by `=`,
/// and return the combined output of the expected failure.
///
/// Needed because [`SetParametersArgs::args()`] emits the flag and value as two separate
/// argv entries, which kopia's parser cannot accept for a negative number (see step 2a).
/// The `=` form is the only way to put a negative value in front of kopia's range
/// validation — which is what the admission messages quote, so it has to be checkable.
fn raw_set_parameters_err(config_dir: &std::path::Path, flag: &str) -> String {
    let out = std::process::Command::new("kopia")
        .args(["repository", "set-parameters", flag])
        .env("KOPIA_PASSWORD", PASSWORD)
        .env("KOPIA_CONFIG_PATH", config_path(config_dir))
        .env("KOPIA_CACHE_DIRECTORY", config_dir.join("cache"))
        .env("KOPIA_LOG_DIR", config_dir.join("logs"))
        .env("KOPIA_CHECK_FOR_UPDATES", "false")
        .output()
        .expect("run kopia repository set-parameters");
    assert!(
        !out.status.success(),
        "{flag} was expected to FAIL, but kopia accepted it: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Read `kopia maintenance info --json` as RAW BYTES.
///
/// The typed [`kopiur_kopia::MaintenanceInfo`] deliberately drops the per-task `runs`
/// history ("its shape is large and unstable"), so a byte-exact claim about `runs` cannot
/// be made through it — this shells out for the unparsed output on purpose.
fn raw_maintenance_info(config_dir: &std::path::Path) -> String {
    let out = std::process::Command::new("kopia")
        .args(["maintenance", "info", "--json"])
        .env("KOPIA_PASSWORD", PASSWORD)
        .env("KOPIA_CONFIG_PATH", config_path(config_dir))
        .env("KOPIA_CACHE_DIRECTORY", config_dir.join("cache"))
        .env("KOPIA_LOG_DIR", config_dir.join("logs"))
        .env("KOPIA_CHECK_FOR_UPDATES", "false")
        .output()
        .expect("run kopia maintenance info --json");
    assert!(
        out.status.success(),
        "maintenance info failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("maintenance info emits UTF-8 JSON")
}

/// The `"runs": …` tail of kopia's own emitted JSON, verbatim.
///
/// `runs` is the last key kopia writes, so everything from the marker onward IS the run
/// history — comparing these slices is a true byte comparison of kopia's output, not of a
/// re-serialized parse. The marker is asserted rather than defaulted: if a future kopia
/// reorders the object, this test must fail loudly instead of silently comparing "".
fn runs_tail(raw: &str) -> &str {
    let at = raw
        .find("\"runs\":")
        .expect("maintenance info --json must carry a `runs` key");
    &raw[at..]
}

#[tokio::test]
#[cfg_attr(not(feature = "integration"), ignore)]
async fn maintenance_set_owner_does_not_rewrite_schedule_runs() {
    let (client, _repo, cfg) = fresh_repo().await;

    // A run history to protect. Without this the whole test is vacuous: a fresh repository
    // reports `"runs": null`, which is trivially identical to itself.
    client
        .maintenance_run(MaintenanceMode::Full)
        .await
        .expect("full maintenance");
    let before = raw_maintenance_info(cfg.path());
    assert!(
        runs_tail(&before).contains("\"success\":true"),
        "the fixture must actually have recorded runs, or this proves nothing: {}",
        runs_tail(&before)
    );

    // (1) `--owner me`, the call kopiur's restamp path makes. It resolves to the current
    // `user@host`, so on a freshly created repository it is also a no-op write — which is
    // why (2) exists.
    client
        .maintenance_set_owner_me()
        .await
        .expect("maintenance set --owner me");
    let after_me = raw_maintenance_info(cfg.path());
    assert_eq!(
        runs_tail(&before),
        runs_tail(&after_me),
        "`maintenance set --owner me` must leave schedule.runs byte-identical"
    );

    // (2) A GENUINE owner change, so the claim is not resting on kopia short-circuiting an
    // unchanged write. This is the version the before/after run-delta work depends on: a
    // restamp that handed the lease to this pod must not disturb the history the delta is
    // measured over.
    client
        .maintenance_set_owner("kopiur-probe@elsewhere")
        .await
        .expect("maintenance set --owner <other>");
    let after_other = raw_maintenance_info(cfg.path());
    assert!(
        after_other.contains("kopiur-probe@elsewhere"),
        "the owner change must have actually taken: {after_other}"
    );
    assert_eq!(
        runs_tail(&before),
        runs_tail(&after_other),
        "a REAL owner change must also leave schedule.runs byte-identical"
    );
}
