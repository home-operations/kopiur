use super::*;
use kopiur_api::common::{CronSpec, RepositoryKind, RepositoryRef};
use kopiur_api::maintenance::RunStatus;
use kopiur_api::{MaintenanceSpec, MaintenanceStatus, Ownership, TakeoverPolicy};

fn maint_with(quick_cron: &str, full_cron: &str, status: Option<MaintenanceStatus>) -> Maintenance {
    let mut m = Maintenance::new(
        "nas-primary",
        MaintenanceSpec {
            repository: RepositoryRef {
                kind: RepositoryKind::Repository,
                name: "nas-primary".into(),
                namespace: None,
            },
            schedule: kopiur_api::MaintenanceSchedule {
                quick: CronSpec {
                    cron: quick_cron.into(),
                    jitter: None,
                    timezone: None,
                },
                full: CronSpec {
                    cron: full_cron.into(),
                    jitter: None,
                    timezone: None,
                },
                timezone: None,
            },
            ownership: Ownership {
                owner: "kopiur/prod/nas-primary".into(),
                owner_aliases: Vec::new(),
                takeover_policy: TakeoverPolicy::Never,
            },
            mover: None,
            failure_policy: None,
            credential_projection: None,
        },
    );
    m.metadata.uid = Some("uid-maint-1".into());
    m.status = status;
    m
}

fn run_at(ts: &str) -> RunStatus {
    RunStatus {
        last_run_at: Some(ts.into()),
        ..Default::default()
    }
}

fn handled_at(ts: &str) -> RunStatus {
    RunStatus {
        last_handled_at: Some(ts.into()),
        ..Default::default()
    }
}

// A fixed mid-slot instant (Saturday 12:02:33 UTC) for due_mode tests that
// anchor lastRunAt/lastHandledAt relative to "now". With a live Utc::now(),
// a run landing in the first second after a cron boundary (e.g. 03:15:00 for
// `*/5 * * * *`) puts a genuinely new slot between the `now - 1s` anchor and
// now, and due_mode rightly fires — a ~1/300 CI flake, not an operator bug.
fn pinned_now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-06-06T12:02:33Z")
        .unwrap()
        .with_timezone(&Utc)
}

// Regression guard for the TTL-reap loop: a YIELDED slot advances
// `lastHandledAt` but never `lastRunAt`. Once the slot's Job self-reaps
// (ttlSecondsAfterFinished), the durable marker — not the Job's existence —
// must keep the slot from re-firing, or a lease-blocked Maintenance spawns
// a yield Job every TTL period forever.
#[test]
fn handled_slot_does_not_refire_after_its_job_is_ttl_reaped() {
    let now = pinned_now();
    let just = (now - chrono::Duration::seconds(1)).to_rfc3339();
    let status = MaintenanceStatus {
        quick: Some(handled_at(&just)),
        full: Some(handled_at(&just)),
        ..Default::default()
    };
    let m = maint_with("*/5 * * * *", "0 3 * * *", Some(status));
    assert!(
        due_mode(&m, now, None).is_none(),
        "a handled (yielded) slot must not re-fire after its Job is TTL-reaped"
    );
}

// The handled anchor must be the OBSERVATION instant, not the slot: a
// first-ever slot sits ~a year back (the lookback fallback), and anchoring
// there leaves the next slot still in the past — a yield-only Maintenance
// would march through the whole historic backlog one Job at a time.
#[test]
fn handling_a_year_old_slot_does_not_start_a_backlog_march() {
    let now = pinned_now();
    // What record_handled_slot writes for the first-ever (year-old) slot:
    // the observation instant `now`, never the slot itself.
    let status = MaintenanceStatus {
        quick: Some(handled_at(&now.to_rfc3339())),
        full: Some(handled_at(&now.to_rfc3339())),
        ..Default::default()
    };
    let m = maint_with("0 3 * * *", "30 4 * * 0", Some(status));
    assert!(
        due_mode(&m, now, None).is_none(),
        "after handling the first-ever slot, the next due slot must be in \
         the FUTURE — not the next entry of a year-long backlog"
    );
}

#[test]
fn mode_after_takes_the_later_of_run_and_handled() {
    let now = Utc::now();
    let old = (now - chrono::Duration::days(3)).to_rfc3339();
    let recent = (now - chrono::Duration::hours(1)).to_rfc3339();
    // Run long ago, handled recently (yield path) → handled wins.
    let status = MaintenanceStatus {
        full: Some(RunStatus {
            last_run_at: Some(old.clone()),
            last_handled_at: Some(recent.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let m = maint_with("*/5 * * * *", "0 3 * * *", Some(status));
    let after = mode_after(&m, MaintenanceMode::Full);
    assert_eq!(after.to_rfc3339(), recent);
    // Handled long ago, run recently (real-run path) → run wins.
    let status = MaintenanceStatus {
        full: Some(RunStatus {
            last_run_at: Some(recent.clone()),
            last_handled_at: Some(old),
            ..Default::default()
        }),
        ..Default::default()
    };
    let m = maint_with("*/5 * * * *", "0 3 * * *", Some(status));
    assert_eq!(mode_after(&m, MaintenanceMode::Full).to_rfc3339(), recent);
    // Neither recorded → first-ever fires immediately (a slot exists in the
    // year-long lookback window).
    let m = maint_with("*/5 * * * *", "0 3 * * *", None);
    assert!(due_mode(&m, now, None).is_some());
}

#[test]
fn first_ever_reconcile_is_due_and_prefers_full() {
    // No status → both due; full wins (it subsumes quick).
    let m = maint_with("*/5 * * * *", "0 3 * * *", None);
    let (mode, _slot) = due_mode(&m, Utc::now(), None).expect("first run is due");
    assert_eq!(mode, MaintenanceMode::Full);
}

#[test]
fn not_due_right_after_a_run() {
    // Both ran one second ago → next slots are in the future → nothing due.
    let now = pinned_now();
    let just = (now - chrono::Duration::seconds(1)).to_rfc3339();
    let status = MaintenanceStatus {
        quick: Some(run_at(&just)),
        full: Some(run_at(&just)),
        ..Default::default()
    };
    let m = maint_with("*/5 * * * *", "0 3 * * *", Some(status));
    assert!(
        due_mode(&m, now, None).is_none(),
        "a mode that just ran must not be immediately due again"
    );
}

#[test]
fn quick_due_when_full_recent() {
    // Full ran moments ago (not due), quick last ran long ago (due) → quick.
    let now = pinned_now();
    let status = MaintenanceStatus {
        quick: Some(run_at(&(now - chrono::Duration::days(2)).to_rfc3339())),
        full: Some(run_at(&(now - chrono::Duration::seconds(1)).to_rfc3339())),
        ..Default::default()
    };
    let m = maint_with("*/5 * * * *", "0 3 * * *", Some(status));
    let (mode, _) = due_mode(&m, now, None).expect("quick should be due");
    assert_eq!(mode, MaintenanceMode::Quick);
}

#[test]
fn job_name_is_deterministic_and_within_limit() {
    let slot = DateTime::parse_from_rfc3339("2026-06-06T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let short = maintenance_job_name("nas-primary", MaintenanceMode::Full, slot);
    assert!(short.len() <= 52);
    assert!(short.starts_with("nas-primary-f-"));
    // Deterministic.
    assert_eq!(
        short,
        maintenance_job_name("nas-primary", MaintenanceMode::Full, slot)
    );
    // Quick vs full differ.
    assert_ne!(
        short,
        maintenance_job_name("nas-primary", MaintenanceMode::Quick, slot)
    );
}

#[test]
fn job_name_truncates_and_hashes_long_cr_names() {
    let slot = DateTime::parse_from_rfc3339("2026-06-06T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let long = "a-very-long-repository-name-that-blows-the-dns-label-budget-easily";
    let n1 = maintenance_job_name(long, MaintenanceMode::Quick, slot);
    assert!(n1.len() <= 52, "got {} ({} chars)", n1, n1.len());
    // Stable across calls (hash is run-independent).
    assert_eq!(n1, maintenance_job_name(long, MaintenanceMode::Quick, slot));
    // A different long name produces a different truncated+hashed name.
    let other = "b-very-long-repository-name-that-blows-the-dns-label-budget-easily";
    assert_ne!(
        n1,
        maintenance_job_name(other, MaintenanceMode::Quick, slot)
    );
}

#[test]
fn requeue_is_capped() {
    // Full daily, last ran moments ago → next full ~24h out, but the requeue
    // is capped so the controller still wakes within the heartbeat.
    let now = Utc::now();
    let status = MaintenanceStatus {
        quick: Some(run_at(&(now - chrono::Duration::seconds(1)).to_rfc3339())),
        full: Some(run_at(&(now - chrono::Duration::seconds(1)).to_rfc3339())),
        ..Default::default()
    };
    let m = maint_with("0 */6 * * *", "0 3 * * *", Some(status));
    assert!(cap(next_wakeup(&m, now, None, None)) <= REQUEUE_CAP);
}

// --- scheduleDefaults.timezone three-level cascade (GitHub #174 item 3) ----
// per-cron `timezone` -> schedule-level `timezone` -> repo `scheduleDefaults.
// timezone` -> UTC.

#[test]
fn due_mode_falls_all_the_way_through_to_the_repo_default_timezone() {
    // No per-cron or schedule-level timezone at all — the repo default is the
    // ONLY source of a non-UTC zone, so it must be what shifts the slot.
    let now = DateTime::parse_from_rfc3339("2026-06-09T05:30:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let status = MaintenanceStatus {
        quick: Some(run_at(&(now - chrono::Duration::hours(2)).to_rfc3339())),
        full: Some(run_at(&(now - chrono::Duration::hours(2)).to_rfc3339())),
        ..Default::default()
    };
    let m = maint_with("0 5 * * *", "0 5 * * *", Some(status));
    assert!(
        due_mode(&m, now, None).is_some(),
        "UTC (no repo default) → 05:00 UTC has already passed"
    );
    assert!(
        due_mode(&m, now, Some("America/Los_Angeles")).is_none(),
        "repo scheduleDefaults.timezone must shift the evaluated slot when no \
         per-cron or schedule-level timezone is set"
    );
}

#[test]
fn schedule_level_timezone_wins_over_repo_default() {
    let now = DateTime::parse_from_rfc3339("2026-06-09T05:30:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let status = MaintenanceStatus {
        quick: Some(run_at(&(now - chrono::Duration::hours(2)).to_rfc3339())),
        full: Some(run_at(&(now - chrono::Duration::hours(2)).to_rfc3339())),
        ..Default::default()
    };
    let mut m = maint_with("0 5 * * *", "0 5 * * *", Some(status));
    m.spec.schedule.timezone = Some("UTC".into());
    // Schedule-level UTC says the slot is due; the repo default
    // (America/Los_Angeles, which would push the slot hours into the future)
    // must be ignored.
    assert!(
        due_mode(&m, now, Some("America/Los_Angeles")).is_some(),
        "schedule-level timezone must win over the repo default"
    );
}

#[test]
fn per_cron_timezone_wins_over_repo_default() {
    let now = DateTime::parse_from_rfc3339("2026-06-09T05:30:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let status = MaintenanceStatus {
        full: Some(run_at(&(now - chrono::Duration::hours(2)).to_rfc3339())),
        ..Default::default()
    };
    let mut m = maint_with("*/5 * * * *", "0 5 * * *", Some(status));
    m.spec.schedule.full.timezone = Some("UTC".into());
    // Per-cron UTC on full wins over BOTH the (absent) schedule-level timezone
    // and the repo default.
    let (mode, _) = due_mode(&m, now, Some("America/Los_Angeles")).expect("full is due");
    assert_eq!(mode, MaintenanceMode::Full);
}

// --- manual (annotation-requested) runs -----------------------------------

fn maint_with_annotations(
    annotations: &[(&str, &str)],
    manual_status: Option<kopiur_api::ManualRunStatus>,
) -> Maintenance {
    let mut m: Maintenance = serde_json::from_value(serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Maintenance",
        "metadata": { "name": "maint", "namespace": "ns" },
        "spec": {
            "repository": { "kind": "Repository", "name": "repo" },
            "schedule": { "quick": { "cron": "0 */6 * * *" }, "full": { "cron": "0 3 * * *" } },
            "ownership": { "owner": "test" }
        }
    }))
    .expect("maintenance fixture");
    if !annotations.is_empty() {
        m.metadata.annotations = Some(
            annotations
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
    }
    if let Some(manual) = manual_status {
        m.status = Some(kopiur_api::MaintenanceStatus {
            manual_run: Some(manual),
            ..Default::default()
        });
    }
    m
}

#[test]
fn manual_run_request_parses_annotations_and_defaults_to_quick() {
    use crate::consts::{RUN_MODE_ANNOTATION, RUN_REQUESTED_ANNOTATION};
    let m = maint_with_annotations(&[(RUN_REQUESTED_ANNOTATION, "2026-06-11T12:00:00Z")], None);
    let (at, mode) = manual_run_request(&m).expect("ok").expect("requested");
    assert_eq!(
        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "2026-06-11T12:00:00Z"
    );
    assert_eq!(
        mode,
        kopiur_api::ManualRunMode::Quick,
        "mode defaults to quick"
    );

    let m = maint_with_annotations(
        &[
            (RUN_REQUESTED_ANNOTATION, "2026-06-11T12:00:00Z"),
            (RUN_MODE_ANNOTATION, "full"),
        ],
        None,
    );
    let (_, mode) = manual_run_request(&m).expect("ok").expect("requested");
    assert_eq!(mode, kopiur_api::ManualRunMode::Full);
}

#[test]
fn manual_run_request_dedupes_an_answered_timestamp_but_not_a_new_one() {
    use crate::consts::RUN_REQUESTED_ANNOTATION;
    let answered = kopiur_api::ManualRunStatus {
        requested_at: Some("2026-06-11T12:00:00Z".into()),
        mode: Some(kopiur_api::ManualRunMode::Quick),
        phase: Some(kopiur_api::ManualRunPhase::Succeeded),
        completed_at: Some("2026-06-11T12:01:00Z".into()),
    };
    // Same timestamp, terminal phase: handled — a no-op.
    let m = maint_with_annotations(
        &[(RUN_REQUESTED_ANNOTATION, "2026-06-11T12:00:00Z")],
        Some(answered.clone()),
    );
    assert!(manual_run_request(&m).expect("ok").is_none());

    // A NEW timestamp re-arms the trigger.
    let m = maint_with_annotations(
        &[(RUN_REQUESTED_ANNOTATION, "2026-06-11T13:00:00Z")],
        Some(answered.clone()),
    );
    assert!(manual_run_request(&m).expect("ok").is_some());

    // A Running phase is NOT deduped here (the reconcile body resolves the
    // in-flight Job / lost-outcome cases).
    let running = kopiur_api::ManualRunStatus {
        phase: Some(kopiur_api::ManualRunPhase::Running),
        completed_at: None,
        ..answered.clone()
    };
    let m = maint_with_annotations(
        &[(RUN_REQUESTED_ANNOTATION, "2026-06-11T12:00:00Z")],
        Some(running),
    );
    assert!(manual_run_request(&m).expect("ok").is_some());

    // A phase written by a NEWER operator is NOT an answer this build can vouch
    // for, so the request is re-driven (idempotent: the Job name is keyed on the
    // request timestamp) rather than silently dropped.
    let unknown = kopiur_api::ManualRunStatus {
        phase: Some(kopiur_api::ManualRunPhase::Unknown("Queued".into())),
        completed_at: None,
        ..answered
    };
    let m = maint_with_annotations(
        &[(RUN_REQUESTED_ANNOTATION, "2026-06-11T12:00:00Z")],
        Some(unknown),
    );
    assert!(manual_run_request(&m).expect("ok").is_some());
}

#[test]
fn manual_run_request_rejects_garbage_with_a_fix() {
    use crate::consts::{RUN_MODE_ANNOTATION, RUN_REQUESTED_ANNOTATION};
    let m = maint_with_annotations(&[(RUN_REQUESTED_ANNOTATION, "yesterday")], None);
    let err = manual_run_request(&m).expect_err("bad timestamp");
    let msg = err.to_string();
    assert!(msg.contains("must be an RFC3339 timestamp"), "{msg}");
    assert!(msg.contains("kubectl kopiur maintenance run"), "{msg}");

    let m = maint_with_annotations(
        &[
            (RUN_REQUESTED_ANNOTATION, "2026-06-11T12:00:00Z"),
            (RUN_MODE_ANNOTATION, "FULL"),
        ],
        None,
    );
    let msg = manual_run_request(&m).expect_err("bad mode").to_string();
    assert!(msg.contains("must be `quick` or `full`"), "{msg}");
}

#[test]
fn manual_job_names_never_collide_with_cron_slot_names() {
    let at = DateTime::parse_from_rfc3339("2026-06-11T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let manual_q = manual_job_name("maint", kopiur_api::ManualRunMode::Quick, at);
    let manual_f = manual_job_name("maint", kopiur_api::ManualRunMode::Full, at);
    let cron_q = maintenance_job_name("maint", MaintenanceMode::Quick, at);
    let cron_f = maintenance_job_name("maint", MaintenanceMode::Full, at);
    let names = [&manual_q, &manual_f, &cron_q, &cron_f];
    let unique: std::collections::BTreeSet<_> = names.iter().collect();
    assert_eq!(unique.len(), 4, "{names:?}");
    assert!(manual_q.contains("-mq-"), "{manual_q}");
    assert!(manual_f.contains("-mf-"), "{manual_f}");
    // Long CR names stay within the budget.
    let long = "m".repeat(80);
    assert!(manual_job_name(&long, kopiur_api::ManualRunMode::Full, at).len() <= 52);
}

/// M6: `spec.ownership.ownerAliases` must ride the mover work spec verbatim —
/// without it, a Maintenance whose lease moved to a cluster-qualified format
/// would see kopia's still-recorded legacy owner as foreign and yield forever.
#[test]
fn maintenance_op_threads_owner_and_aliases_from_ownership() {
    let mut m = maint_with("0 */6 * * *", "0 3 * * *", None);
    m.spec.ownership.owner = "kopiur/east/prod/nas-primary".into();
    m.spec.ownership.owner_aliases = vec!["kopiur/prod/nas-primary".into()];
    m.spec.ownership.takeover_policy = TakeoverPolicy::PromptCondition;
    let op = maintenance_op(&m, MaintenanceMode::Full);
    assert_eq!(op.mode, MaintenanceMode::Full);
    assert_eq!(op.owner, "kopiur/east/prod/nas-primary");
    assert_eq!(
        op.owner_aliases,
        vec!["kopiur/prod/nas-primary".to_string()]
    );
    assert_eq!(op.takeover_policy, TakeoverPolicy::PromptCondition);

    // No aliases configured (the pre-M6 shape): the op carries none.
    let plain = maint_with("0 */6 * * *", "0 3 * * *", None);
    assert!(
        maintenance_op(&plain, MaintenanceMode::Quick)
            .owner_aliases
            .is_empty()
    );
}
