//! Unit tests parsing the captured kopia 0.23 fixture JSON into the typed
//! models. These assert the key fields the operator depends on, and prove the
//! `camelCase` / explicit-rename mapping matches kopia's actual keys.

use kopiur_kopia::{
    MaintenanceInfo, MaintenanceRun, MaintenanceRunExtra, RepositoryStatus, SnapshotCreateResult,
    SnapshotListEntry, reclaimed_bytes_since,
};

const SNAPSHOT_CREATE: &str = include_str!("fixtures/snapshot_create.json");
const SNAPSHOT_LIST: &str = include_str!("fixtures/snapshot_list.json");
const REPOSITORY_STATUS: &str = include_str!("fixtures/repository_status.json");
const MAINTENANCE_INFO: &str = include_str!("fixtures/maintenance_info.json");
/// `kopia maintenance info --json` captured from a REAL kopia 0.23.1 filesystem
/// repository immediately BEFORE a `maintenance run --full --safety=none`, and
/// the matching capture immediately after. The run deleted one snapshot's worth
/// of packs, so `full-delete-blobs` reports genuinely-freed bytes in the second.
/// Hand-writing these would defeat their purpose: every key asserted below is a
/// claim about kopia's output, not about ours.
const MAINTENANCE_INFO_BEFORE_FULL: &str =
    include_str!("fixtures/maintenance_info_before_full.json");
const MAINTENANCE_INFO_AFTER_FULL: &str = include_str!("fixtures/maintenance_info_after_full.json");

/// The bytes kopia actually freed in the captured full run: `full-delete-blobs`
/// deleted 6 packs totalling this much (`deleteUnreferencedPacksStats
/// .deletedTotalSize`). Note it is NOT `snapshotGCStats.deletedContentSize`
/// (6_000_529 in the same capture) — see [`reclaimed_bytes_since`].
const CAPTURED_FULL_RECLAIMED: i64 = 14_022_073;

#[test]
fn parse_snapshot_create() {
    let r: SnapshotCreateResult = serde_json::from_str(SNAPSHOT_CREATE).unwrap();
    assert_eq!(r.id, "edf6ef74ec18dffc79e26907e0c3c7fc");
    assert_eq!(r.source.user_name, "root");
    assert_eq!(r.source.host, "desktop-8emkv7q");
    assert_eq!(r.source.path, "/tmp/claude-0/tmp.Zz62PJwR0o");
    assert_eq!(
        r.source.identity(),
        "root@desktop-8emkv7q:/tmp/claude-0/tmp.Zz62PJwR0o"
    );
    // Stats come from rootEntry.summ on the create result.
    assert_eq!(r.total_bytes(), 12);
    assert_eq!(r.file_count(), 2);
    assert_eq!(r.error_count(), 0);
    assert!(r.end_time >= r.start_time);
}

#[test]
fn parse_snapshot_list() {
    let entries: Vec<SnapshotListEntry> = serde_json::from_str(SNAPSHOT_LIST).unwrap();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.id, "edf6ef74ec18dffc79e26907e0c3c7fc");
    assert_eq!(e.source.path, "/tmp/claude-0/tmp.Zz62PJwR0o");
    // The `stats` block on list entries.
    assert_eq!(e.stats.total_size, 12);
    assert_eq!(e.stats.file_count, 2);
    assert_eq!(e.stats.non_cached_files, 2);
    assert_eq!(e.stats.cached_files, 0);
    assert_eq!(e.stats.dir_count, 2);
    assert_eq!(e.stats.error_count, 0);
    // GFS retention reasons.
    assert!(e.retention_reason.contains(&"latest-1".to_string()));
    assert!(e.retention_reason.contains(&"daily-1".to_string()));
}

#[test]
fn parse_repository_status() {
    let s: RepositoryStatus = serde_json::from_str(REPOSITORY_STATUS).unwrap();
    // uniqueIDHex is the stable repo identity (explicit rename).
    assert_eq!(
        s.unique_id_hex,
        "e9f0365501231565390cc1327cb9665c80dd54ff41ce0419b0ccaef69a24f021"
    );
    assert_eq!(s.client_options.hostname, "desktop-8emkv7q");
    assert_eq!(s.client_options.username, "root");
    assert_eq!(s.storage.storage_type, "filesystem");
    assert_eq!(s.content_format.hash, "BLAKE2B-256-128");
    assert_eq!(s.content_format.encryption, "AES256-GCM-HMAC-SHA256");
    assert_eq!(s.content_format.version, 3);
    // Backend config stays opaque but is preserved.
    assert_eq!(
        s.storage.config.get("path").and_then(|v| v.as_str()),
        Some("/tmp/claude-0/tmp.xo1UiqvExG")
    );
    // This fixture is REAL kopia output from a filesystem repo, which cannot object-lock —
    // so kopia emits a bare `"blobRetention": {}` (both inner keys are omitempty). That is
    // the "retention is off" observation, and it must decode as such rather than as absent.
    // The enabled shape is covered by `blob_retention_enabled_shape` below with inline JSON,
    // because no local backend can produce it and faking it in the fixture would falsify a
    // file whose whole contract is "verbatim kopia output".
    let retention = s
        .blob_retention
        .expect("kopia 0.23 always emits blobRetention");
    assert!(!retention.is_enabled());
    assert_eq!(retention.mode, "");
    assert_eq!(retention.period_ns, 0);
}

#[test]
fn blob_retention_enabled_shape() {
    // 720h == 30 days == 2_592_000_000_000_000ns. kopia reports the period as a Go
    // time.Duration, so the unit is NANOSECONDS — reading it as seconds would understate
    // the window by a factor of a billion and make the drift comparator re-apply forever.
    let s: RepositoryStatus = serde_json::from_str(
        r#"{
            "configFile": "/config/repository.config",
            "uniqueIDHex": "deadbeef",
            "clientOptions": {"hostname": "h", "username": "u"},
            "storage": {"type": "s3", "config": {"bucket": "b"}},
            "contentFormat": {"hash": "BLAKE2B-256-128", "encryption": "AES256-GCM-HMAC-SHA256", "version": 3},
            "blobRetention": {"retentionMode": "GOVERNANCE", "retentionPeriod": 2592000000000000}
        }"#,
    )
    .unwrap();
    let r = s.blob_retention.expect("blobRetention present");
    assert!(r.is_enabled());
    assert_eq!(r.mode, "GOVERNANCE");
    assert_eq!(r.period_ns, 2_592_000_000_000_000);

    // kopia treats a half-set config as OFF (`IsRetentionEnabled` requires both) — mirror
    // that exactly, or kopiur would report protection that does not exist.
    let half = kopiur_kopia::BlobRetention {
        mode: "GOVERNANCE".into(),
        period_ns: 0,
    };
    assert!(!half.is_enabled());
    let half = kopiur_kopia::BlobRetention {
        mode: String::new(),
        period_ns: 2_592_000_000_000_000,
    };
    assert!(!half.is_enabled());
}

#[test]
fn parse_maintenance_info() {
    let m: MaintenanceInfo = serde_json::from_str(MAINTENANCE_INFO).unwrap();
    assert_eq!(m.owner, "root@desktop-8emkv7q");
    assert!(m.quick.enabled);
    assert!(m.full.enabled);
    assert_eq!(m.quick.interval, 3_600_000_000_000);
    assert_eq!(m.full.interval, 86_400_000_000_000);
    let sched = m.schedule.expect("schedule present");
    assert!(sched.next_full_maintenance.is_some());
    assert!(sched.next_quick_maintenance.is_some());
}

/// The `runs` history the previous test's `schedule` block also carries: kopia
/// DOES emit it (`cli/command_maintenance_info.go` embeds `maintenance.Schedule`),
/// and the operator's reclaimed-bytes figure is a diff over it.
#[test]
fn parse_maintenance_info_runs_history() {
    let m: MaintenanceInfo = serde_json::from_str(MAINTENANCE_INFO).unwrap();
    let sched = m.schedule.expect("schedule present");
    let gc = sched.runs.get("snapshot-gc").expect("snapshot-gc history");
    assert_eq!(gc.len(), 1);
    assert!(gc[0].success);
    assert!(gc[0].start.is_some());
    assert!(gc[0].end.is_some());
    // The extras decode by `kind`, and an extra we have no typed accessor for
    // (`snapshotGCStats`) still decodes rather than failing the parse.
    assert_eq!(gc[0].extra.len(), 1);
    assert_eq!(gc[0].extra[0].kind, "snapshotGCStats");
    assert_eq!(gc[0].extra[0].data["deletedContentSize"], 0);
    // ...and it is NOT reachable through a reclaimed-bytes accessor.
    assert!(gc[0].delete_unreferenced_packs_stats().is_none());
}

/// A fresh repository emits `"runs": null` (the Go field carries no
/// `omitempty`), which must decode to an empty map rather than failing the
/// parse of a `maintenance info` the mover's lease decision depends on.
#[test]
fn null_runs_history_decodes_as_empty() {
    let json = r#"{"owner":"u@h","quick":{"enabled":true,"interval":1},
        "full":{"enabled":false,"interval":0},
        "schedule":{"nextFullMaintenance":"2026-06-02T20:13:59Z",
                    "nextQuickMaintenance":"2026-06-01T21:13:59Z","runs":null}}"#;
    let m: MaintenanceInfo = serde_json::from_str(json).unwrap();
    assert!(m.schedule.expect("schedule").runs.is_empty());
}

/// `RunInfo.Success` is `json:"success,omitempty"`, so a FAILED run serializes
/// with the key absent — and `extra` is only populated on success. A failed run
/// must therefore decode as `success: false` and contribute nothing.
#[test]
fn failed_run_omits_success_and_is_not_counted() {
    let before: MaintenanceInfo = serde_json::from_str(
        r#"{"quick":{"enabled":true,"interval":1},"full":{"enabled":true,"interval":1},
            "schedule":{"runs":{}}}"#,
    )
    .unwrap();
    let after: MaintenanceInfo = serde_json::from_str(
        r#"{"quick":{"enabled":true,"interval":1},"full":{"enabled":true,"interval":1},
            "schedule":{"runs":{"full-delete-blobs":[
              {"start":"2026-06-01T20:00:00Z","end":"2026-06-01T20:00:01Z",
               "error":"unable to delete blob: permission denied"}]}}}"#,
    )
    .unwrap();
    let run = &after.schedule.as_ref().unwrap().runs["full-delete-blobs"][0];
    assert!(!run.success, "absent `success` means failed");
    assert_eq!(run.error, "unable to delete blob: permission denied");
    assert!(run.extra.is_empty());
    // Unknown, not zero: a failed run reclaimed an unmeasured amount.
    assert_eq!(reclaimed_bytes_since(&before, &after), None);
}

/// An `extra` kind a newer kopia invents must decode (no serde tag), and
/// `"data": null` — which kopia really does emit for the statless tasks
/// (`compactSingleEpochStats` in the captured fixture) — must not fail either.
#[test]
fn unknown_extra_kinds_and_null_data_decode() {
    let m: MaintenanceInfo = serde_json::from_str(MAINTENANCE_INFO_AFTER_FULL).unwrap();
    let runs = &m.schedule.as_ref().unwrap().runs;
    let single = &runs["compact-single-epoch"][0];
    assert_eq!(single.extra[0].kind, "compactSingleEpochStats");
    assert!(
        single.extra[0].data.is_null(),
        "kopia emits data: null here"
    );

    let future: MaintenanceRun = serde_json::from_str(
        r#"{"start":"2026-06-01T20:00:00Z","end":"2026-06-01T20:00:01Z","success":true,
            "extra":[{"kind":"teleportPacksStats","data":{"teleported":7}}]}"#,
    )
    .unwrap();
    assert_eq!(future.extra[0].kind, "teleportPacksStats");
    assert!(future.delete_unreferenced_packs_stats().is_none());
}

/// The end-to-end claim of C5, against the two REAL captures: the delta over the
/// run history is the figure kopia's own `full-delete-blobs` reported.
#[test]
fn reclaimed_bytes_from_real_before_after_captures() {
    let before: MaintenanceInfo = serde_json::from_str(MAINTENANCE_INFO_BEFORE_FULL).unwrap();
    let after: MaintenanceInfo = serde_json::from_str(MAINTENANCE_INFO_AFTER_FULL).unwrap();

    // The premise: `before` already has a run history (kopia runs maintenance at
    // create time), so this is a genuine delta, not a first-observation shortcut.
    assert!(!before.schedule.as_ref().unwrap().runs.is_empty());
    let packs = after.schedule.as_ref().unwrap().runs["full-delete-blobs"][0]
        .delete_unreferenced_packs_stats()
        .expect("deleteUnreferencedPacksStats");
    assert_eq!(packs.deleted_pack_count, 6);
    assert_eq!(packs.deleted_total_size, CAPTURED_FULL_RECLAIMED);

    assert_eq!(
        reclaimed_bytes_since(&before, &after),
        Some(CAPTURED_FULL_RECLAIMED)
    );
    // Nothing new happened between identical observations.
    assert_eq!(reclaimed_bytes_since(&after, &after), None);
    // The `snapshot-gc` figure is present in the capture and deliberately
    // excluded: it marks contents deleted, it frees no storage.
    let gc_marked =
        after.schedule.as_ref().unwrap().runs["snapshot-gc"][0].extra[0].data["deletedContentSize"]
            .as_i64()
            .expect("snapshotGCStats.deletedContentSize");
    assert_eq!(gc_marked, 6_000_529);
    assert_ne!(
        reclaimed_bytes_since(&before, &after),
        Some(CAPTURED_FULL_RECLAIMED + gc_marked),
        "snapshot-gc must never inflate the reclaimed figure"
    );
}

/// kopia PREPENDS each run and truncates the history to 50 entries per task, so
/// at saturation `len(after) == len(before)` and any index- or length-based diff
/// silently reports nothing. The delta must key on `start`.
#[test]
fn saturated_fifty_entry_history_still_finds_the_new_run() {
    fn hist(starts: &[i64], bytes: i64) -> MaintenanceInfo {
        let runs: Vec<MaintenanceRun> = starts
            .iter()
            .map(|m| MaintenanceRun {
                start: Some(
                    chrono::DateTime::parse_from_rfc3339(&format!("2026-06-01T20:{m:02}:00Z"))
                        .unwrap()
                        .into(),
                ),
                end: None,
                success: true,
                error: String::new(),
                extra: vec![MaintenanceRunExtra {
                    kind: "deleteUnreferencedPacksStats".into(),
                    data: serde_json::json!({"deletedTotalSize": bytes}),
                }],
            })
            .collect();
        let mut info: MaintenanceInfo = serde_json::from_str(
            r#"{"quick":{"enabled":true,"interval":1},"full":{"enabled":true,"interval":1},
                "schedule":{"runs":{}}}"#,
        )
        .unwrap();
        info.schedule
            .as_mut()
            .unwrap()
            .runs
            .insert("full-delete-blobs".into(), runs);
        info
    }

    // 50 runs at minutes 0..50 (kopia's cap), newest first, each reporting 1 KiB.
    let older: Vec<i64> = (0..50).rev().collect();
    let before = hist(&older, 1024);
    // The new run is PREPENDED and the oldest entry drops off: same length.
    let mut newer = vec![50];
    newer.extend((1..50).rev());
    let after = hist(&newer, 1024);
    assert_eq!(
        after.schedule.as_ref().unwrap().runs["full-delete-blobs"].len(),
        before.schedule.as_ref().unwrap().runs["full-delete-blobs"].len(),
        "the premise: a saturated history does not grow"
    );
    // Exactly ONE new run, so exactly one run's worth of bytes.
    assert_eq!(reclaimed_bytes_since(&before, &after), Some(1024));
}

/// A quick run on an EPOCH-enabled repository — the kind #458 is about — runs
/// only `compact-single-epoch` + `advance-epoch` (`runQuickMaintenance`
/// short-circuits), neither of which deletes a blob. `None` is the honest
/// answer; writing `0` would claim we measured nothing reclaimed.
#[test]
fn quick_epoch_run_reclaims_nothing_measurable() {
    let before: MaintenanceInfo = serde_json::from_str(MAINTENANCE_INFO_BEFORE_FULL).unwrap();
    let mut after = before.clone();
    let sched = after.schedule.as_mut().unwrap();
    for task in ["compact-single-epoch", "advance-epoch"] {
        let run = MaintenanceRun {
            start: Some(
                chrono::DateTime::parse_from_rfc3339("2036-01-01T00:00:00Z")
                    .unwrap()
                    .into(),
            ),
            success: true,
            ..Default::default()
        };
        sched
            .runs
            .entry(task.to_string())
            .or_default()
            .insert(0, run);
    }
    assert_eq!(reclaimed_bytes_since(&before, &after), None);
}

/// `Some(0)` and `None` are different answers and must stay different: a
/// counted task that ran and freed nothing is a MEASUREMENT of zero.
#[test]
fn measured_zero_is_not_unknown() {
    let before: MaintenanceInfo = serde_json::from_str(
        r#"{"quick":{"enabled":true,"interval":1},"full":{"enabled":true,"interval":1},
            "schedule":{"runs":{}}}"#,
    )
    .unwrap();
    let mut after = before.clone();
    after.schedule.as_mut().unwrap().runs.insert(
        "quick-delete-blobs".into(),
        vec![MaintenanceRun {
            start: Some(
                chrono::DateTime::parse_from_rfc3339("2026-06-01T20:00:00Z")
                    .unwrap()
                    .into(),
            ),
            success: true,
            extra: vec![MaintenanceRunExtra {
                kind: "deleteUnreferencedPacksStats".into(),
                data: serde_json::json!({"deletedPackCount": 0, "deletedTotalSize": 0}),
            }],
            ..Default::default()
        }],
    );
    assert_eq!(reclaimed_bytes_since(&before, &after), Some(0));
    assert_eq!(reclaimed_bytes_since(&before, &before), None);
}

#[test]
fn tolerates_unknown_fields() {
    // Kopia adds fields across releases; we must not reject them.
    let json = r#"{
        "id":"x","source":{"host":"h","userName":"u","path":"/p","futureField":1},
        "startTime":"2026-06-02T03:13:59Z","endTime":"2026-06-02T03:14:00Z",
        "brandNewTopLevelField":true
    }"#;
    let r: SnapshotCreateResult = serde_json::from_str(json).unwrap();
    assert_eq!(r.id, "x");
    assert_eq!(r.total_bytes(), 0); // no rootEntry → defaults
}

/// The epoch parameters kopia 0.23 actually reports (#258). This block is the one place in
/// `model.rs` that does NOT follow the crate's `rename_all = "camelCase"` convention —
/// kopia serializes the Go struct's field names verbatim, with no consistent rule between
/// them — so the per-field renames are exactly what this pins.
#[test]
fn parse_repository_status_epoch_parameters() {
    let s: RepositoryStatus = serde_json::from_str(REPOSITORY_STATUS).unwrap();
    let e = s
        .content_format
        .epoch_parameters
        .expect("kopia 0.23 reports contentFormat.epochParameters");

    assert!(e.enabled);
    // Durations are Go time.Duration NANOSECONDS, not seconds or millis.
    assert_eq!(
        e.min_epoch_duration_ns, 86_400_000_000_000,
        "kopia's 24h default — the advance gate #258 is about"
    );
    assert_eq!(e.epoch_refresh_frequency_ns, 1_200_000_000_000, "20m");
    assert_eq!(e.cleanup_safety_margin_ns, 14_400_000_000_000, "4h");
    assert_eq!(e.advance_on_count, 20);
    assert_eq!(e.checkpoint_frequency, 7);
    assert_eq!(e.delete_parallelism, 4);
    // The size threshold is reported in BYTES while the flag that sets it is named
    // `--epoch-advance-on-size-mb` and means MiB. 10485760 == 10 * 1024 * 1024 — proof
    // the conversion is 1048576, not 1e6. Dividing by the wrong constant makes the drift
    // comparison never converge, re-running set-parameters on every bootstrap forever.
    assert_eq!(e.advance_on_total_size_bytes, 10_485_760);
    assert_eq!(e.advance_on_total_size_bytes / (1024 * 1024), 10);
}

/// A pre-epoch (or trimmed) status must still parse — `epochParameters` is `Option`, and a
/// serde error here would take down the whole bootstrap.
#[test]
fn repository_status_without_epoch_parameters_still_parses() {
    let s: RepositoryStatus = serde_json::from_str(
        r#"{"configFile":"/c","uniqueIDHex":"ab","clientOptions":{},"storage":{},
            "contentFormat":{"hash":"BLAKE2B-256-128","encryption":"AES256-GCM-HMAC-SHA256","version":1}}"#,
    )
    .unwrap();
    assert!(s.content_format.epoch_parameters.is_none());
}
