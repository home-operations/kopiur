//! The `SnapshotSchedule` reconciler — *when* a backup runs (ADR §4.1).
//!
//! ## Timing: requeue-based, not a `tokio::interval` task (decision)
//!
//! We compute the next wall-clock slot during each reconcile and return
//! `Action::requeue(time_until_slot)`. When that requeue fires, we check whether
//! the slot is due and, if so, create a `Snapshot` CR; then we recompute and
//! requeue again. This is **HA-safe and restart-safe**: there is no per-schedule
//! background task to leak, leader election ([`crate::leader`], `--leader-elect`,
//! on by default in the chart) ensures only the Lease-holding replica runs any
//! reconciler at all, and a restart simply recomputes the same wall-clock slot.
//! A `tokio::interval` task per schedule would duplicate across replicas and
//! strand on restart. (ADR §4.1 anchors on `cron(now)`.)
//!
//! The scheduling kernel here is **pure**: [`next_fire`] computes the jittered
//! next slot deterministically (reusing `api::jitter`), and [`should_fire_now`]
//! / [`concurrency_allows`] are clock-free decisions, so they are unit-tested
//! without a cluster.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use kube::api::{ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::reflector::ObjectRef;
use kube::{Api, ResourceExt};

use kopiur_api::common::{
    DeletionPolicy, PolicyRef, RepositoryRef, ScheduleDeletePolicy, TimezoneAmbiguity,
    effective_timezone, repo_key, resolve_tz,
};
use kopiur_api::snapshot::{PrunedBy, SnapshotSpec};
use kopiur_api::{
    ConcurrencyPolicy, ScheduleSpec, Snapshot, SnapshotPolicy, SnapshotSchedule, jitter, validate,
};
use std::collections::BTreeMap;

use crate::consts::ORIGIN_LABEL;
use crate::context::Context;
use crate::error::{Error, Result, error_policy_for};
use crate::io;

/// Requeue while a `SnapshotSchedule` is held by a run whose phase this build
/// cannot read (`ScheduleRunnable=False`). The ordinary concurrency wait
/// requeues at the 1s floor because it expects the run to finish in seconds;
/// this hold cannot clear until an operator acts, so a 1s cadence would spin the
/// reconciler (and re-log) forever. 10 min keeps the warning recurring often
/// enough to be noticed without becoming the noise.
const UNREADABLE_RUN_HOLD_REQUEUE: StdDuration = StdDuration::from_secs(10 * 60);

/// Parse Go-style duration strings used in the CRD (`30m`, `1h`, `90s`).
/// Re-exported from `kopiur-api` so the admission validator and every
/// reconciler (schedules, maintenance, replication, restore `waitTimeout`)
/// parse the exact same grammar.
pub use kopiur_api::parse_go_duration;

/// Compute the next fire time at or after `after`, applying deterministic
/// jitter (ADR §4.1). `H` tokens are resolved first via `jitter::substitute_h`,
/// then the cron's next slot is found, then a per-`(seed, slot)` offset within
/// the `jitter` window is added.
///
/// `seed` should be the schedule's UID (stable across replicas/restarts).
/// Returns an [`Error::InvalidSchedule`] if the (post-substitution) cron fails
/// to parse — defensive, since the webhook validates shape at admission.
pub fn next_fire(
    cron_expr: &str,
    jitter_window: Option<StdDuration>,
    seed: &str,
    after: DateTime<Utc>,
    tz: Tz,
) -> Result<DateTime<Utc>> {
    let resolved = jitter::substitute_h(cron_expr, seed);
    let cron = jitter::cron_parser()
        .parse(&resolved)
        .map_err(|e| Error::InvalidSchedule(format!("{resolved}: {e}")))?;
    // Evaluate the cron in the target zone so a wall-clock field like `0 2 * * *`
    // means 02:00 *there* (DST-correct), then convert the chosen instant back to UTC
    // for storage/requeue. Jitter is a tz-independent offset on the resolved instant.
    let after_local = after.with_timezone(&tz);
    let slot = cron
        .find_next_occurrence(&after_local, false)
        .map_err(|e| Error::InvalidSchedule(format!("no next occurrence for {resolved}: {e}")))?;
    let offset = match jitter_window {
        Some(w) => jitter::offset(seed, slot.timestamp(), w),
        None => StdDuration::ZERO,
    };
    let slot =
        slot + chrono::Duration::from_std(offset).unwrap_or_else(|_| chrono::Duration::zero());
    Ok(slot.with_timezone(&Utc))
}

/// Whether a slot is due to fire at `now` (i.e. the scheduled time has arrived).
pub fn should_fire_now(slot: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now >= slot
}

/// **Pure.** Whether a pinned `nextSchedule` slot must be recomputed because the
/// effective cron timezone changed since it was pinned. `pinned_tz` is the zone
/// recorded on the pin (`None` on legacy pins written before the field existed);
/// `effective` is the zone resolved this reconcile.
///
/// Determinism guard: returns `false` for equal zones AND for an absent `pinned_tz`,
/// so the steady state never recomputes — no jitter churn on every reconcile, and
/// no one-time churn for schedules upgraded across the addition of the field. A
/// recompute is triggered only by an observed, recorded zone that actually differs.
pub fn pin_needs_recompute(pinned_tz: Option<&str>, effective: Tz) -> bool {
    pinned_tz.is_some_and(|p| p != effective.name())
}

/// Outcome of resolving a `SnapshotSchedule`'s effective cron timezone for one
/// reconcile. Distinguishes a genuine resolution (referents read successfully) from a
/// **degraded** pass (a referent GET/list failed, or a matched policy/repo was
/// missing) so the caller can honor the invariant that *a transient referent failure
/// must never invalidate an established pin*: without this distinction the old
/// `(Tz::UTC, None)`-on-failure return was indistinguishable from a genuinely-resolved
/// UTC, so an apiserver blip would flap a `Europe/Berlin` pin to UTC timing and back.
/// Internal to the reconciler — not serialized, so the status schema is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TimezoneResolution {
    /// Referents were read; `tz` is the effective zone and `ambiguity` is `Some` when
    /// matched policies' repositories disagreed on their default (UTC in effect + a
    /// warn-only status condition).
    Resolved {
        tz: Tz,
        ambiguity: Option<TimezoneAmbiguity>,
    },
    /// Resolution could not complete this reconcile (referent GET/list failure or a
    /// missing policy/repo). The controller keeps an established pin untouched and
    /// only self-heals a *first* pin to UTC.
    Degraded,
}

/// **Pure.** Decide the effective zone, ambiguity signal, and whether the pinned
/// `nextSchedule` slot must be recomputed, given the pin's recorded zone
/// (`pinned_tz`) and this reconcile's [`TimezoneResolution`]. Exhaustive over the
/// resolution — no `_ =>`:
///
/// - `Resolved { tz, ambiguity }`: the pin is invalidated iff its recorded zone
///   differs from `tz` (via [`pin_needs_recompute`]); `tz`/`ambiguity` flow on to the
///   re-pin and status.
/// - `Degraded`: a transient referent failure must **never** invalidate an
///   established pin, so recompute is always `false` and the pin's own recorded zone
///   stays in effect for this reconcile (a legacy pin with no recorded zone resolves
///   to UTC via [`resolve_tz`]). No ambiguity is asserted while degraded.
fn resolve_pinned_slot_tz(
    pinned_tz: Option<&str>,
    resolution: &TimezoneResolution,
) -> (Tz, Option<TimezoneAmbiguity>, bool) {
    match resolution {
        TimezoneResolution::Resolved { tz, ambiguity } => {
            (*tz, ambiguity.clone(), pin_needs_recompute(pinned_tz, *tz))
        }
        TimezoneResolution::Degraded => (resolve_tz(pinned_tz), None, false),
    }
}

/// **Pure.** The zone + ambiguity to pin on the FIRST reconcile (no pin recorded
/// yet). Exhaustive over [`TimezoneResolution`] — no `_ =>`:
/// - `Resolved { tz, ambiguity }`: pin that zone (and surface any ambiguity).
/// - `Degraded`: self-heal by pinning UTC now; once referents recover, the
///   pinned-slot branch recomputes into the inherited zone exactly once (then
///   stabilizes — see [`resolve_pinned_slot_tz`]).
fn first_pin_tz(resolution: &TimezoneResolution) -> (Tz, Option<TimezoneAmbiguity>) {
    match resolution {
        TimezoneResolution::Resolved { tz, ambiguity } => (*tz, ambiguity.clone()),
        TimezoneResolution::Degraded => (Tz::UTC, None),
    }
}

/// The `TimezoneDefaultAmbiguous` condition's current truthiness on `schedule.status`
/// (absent condition = not ambiguous). Lets the not-due path skip a status patch when
/// the freshly-computed ambiguity state already matches what's recorded — steady state
/// stays patch-free (no watch churn), while a resolved or newly-arisen ambiguity is
/// corrected promptly instead of lingering until the next fire.
fn recorded_tz_ambiguous(schedule: &SnapshotSchedule) -> bool {
    schedule
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or_default()
        .iter()
        .find(|c| c.type_ == crate::consts::SCHEDULE_TIMEZONE_AMBIGUOUS_CONDITION)
        .is_some_and(|c| c.status == "True")
}

/// Whether the `starting_deadline_seconds` has been missed for a slot (the slot
/// is too old to still run). `None` deadline means "never expires."
pub fn missed_deadline(
    slot: DateTime<Utc>,
    now: DateTime<Utc>,
    starting_deadline_seconds: Option<i64>,
) -> bool {
    match starting_deadline_seconds {
        Some(d) => (now - slot).num_seconds() > d,
        None => false,
    }
}

/// Whether a new run may start given the concurrency policy and whether a run
/// is currently active. `Forbid` skips when active; `Allow`/`Replace` proceed
/// (`Replace`'s cancel-the-old behavior is the caller's IO responsibility).
pub fn concurrency_allows(policy: ConcurrencyPolicy, run_active: bool) -> bool {
    match policy {
        ConcurrencyPolicy::Forbid => !run_active,
        ConcurrencyPolicy::Allow | ConcurrencyPolicy::Replace => true,
    }
}

/// The three-way outcome for a pinned slot. An exhaustive `match` on this is what
/// keeps the reconciler honest: the old boolean `should_create_backup` collapsed
/// "expired past `startingDeadlineSeconds`" into "don't fire", and the caller's
/// wait branch then computed `(slot - now)` for a PAST slot — a 1-second requeue
/// loop, forever, with `nextSchedule` stuck on the expired slot (#345 / M1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotDisposition {
    /// The slot is due and may fire now.
    Fire,
    /// The slot is due but expired past `startingDeadlineSeconds`: skip it and
    /// re-pin `nextSchedule` forward (mirrors CronJob missed-slot semantics).
    /// Deadline expiry deliberately wins over `Forbid`+active: a slot a long
    /// run pushed past its deadline is skipped, not queued.
    SkipExpired,
    /// Not due yet, suspended, or held by `concurrencyPolicy: Forbid` — keep the
    /// pin and wait. A `Forbid`-held due slot (no deadline) stays pinned and
    /// fires when the active run finishes: the one-shot catch-up.
    Wait,
}

/// What to do with the pinned slot right now, combining `suspend`, the slot being
/// due, the deadline, and concurrency. Pure decision.
pub fn slot_disposition(
    schedule: &ScheduleSpec,
    slot: DateTime<Utc>,
    now: DateTime<Utc>,
    run_active: bool,
) -> SlotDisposition {
    if schedule.suspend {
        return SlotDisposition::Wait;
    }
    if !should_fire_now(slot, now) {
        return SlotDisposition::Wait;
    }
    if missed_deadline(slot, now, schedule.starting_deadline_seconds) {
        return SlotDisposition::SkipExpired;
    }
    if concurrency_allows(schedule.concurrency_policy, run_active) {
        SlotDisposition::Fire
    } else {
        SlotDisposition::Wait
    }
}

/// Whether the schedule should produce any `Snapshot` at all right now. Thin
/// boolean view over [`slot_disposition`] (kept for call sites and tests that
/// only care about the fire decision).
pub fn should_create_backup(
    schedule: &ScheduleSpec,
    slot: DateTime<Utc>,
    now: DateTime<Utc>,
    run_active: bool,
) -> bool {
    slot_disposition(schedule, slot, now, run_active) == SlotDisposition::Fire
}

/// Whether a `SnapshotPolicy` with the given `labels` matches a `policySelector`
/// (ADR-0005 §10). Pure decision (the `Api::list` IO is the caller's). Implements
/// `matchLabels` (every key must be present with the required value) plus the
/// common `matchExpressions` operators (`In`/`NotIn`/`Exists`/`DoesNotExist`); an
/// empty selector matches every policy. A suspended policy is the caller's concern.
pub fn policy_matches_selector(
    labels: &BTreeMap<String, String>,
    selector: &k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector,
) -> bool {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement;
    if let Some(ml) = &selector.match_labels {
        for (k, v) in ml {
            if labels.get(k) != Some(v) {
                return false;
            }
        }
    }
    if let Some(exprs) = &selector.match_expressions {
        for LabelSelectorRequirement {
            key,
            operator,
            values,
        } in exprs
        {
            let vals = values.clone().unwrap_or_default();
            let present = labels.get(key);
            let ok = match operator.as_str() {
                "In" => present.is_some_and(|v| vals.iter().any(|x| x == v)),
                "NotIn" => present.is_none_or(|v| !vals.iter().any(|x| x == v)),
                "Exists" => present.is_some(),
                "DoesNotExist" => present.is_none(),
                // Unknown operator: the schema constrains the set; treat as no constraint.
                _ => true,
            };
            if !ok {
                return false;
            }
        }
    }
    true
}

/// Whether a freshly-created schedule should fire one backup immediately on
/// creation (`runOnCreate`), rather than waiting for the first cron slot. Pure
/// decision: true only when `runOnCreate` is set, the schedule is not suspended,
/// and no run has happened yet. The `already_ran` guard makes it idempotent —
/// once the first run is recorded in `status.lastSchedule`, this returns false,
/// so a retried/re-entered first reconcile never double-fires.
pub fn should_run_on_create(schedule: &ScheduleSpec, already_ran: bool) -> bool {
    schedule.run_on_create && !schedule.suspend && !already_ran
}

/// The kstatus Ready conditions for a `SnapshotSchedule` (ADR-0005 §2). A schedule
/// has no phase; it's `Ready` whenever it has reconciled — whether actively
/// scheduling or correctly `suspend`ed (a paused schedule is healthy, not stalled).
/// `ambiguity` is `Some` when the schedule inherits its timezone but its target
/// policies' repositories disagree on the zone (UTC in effect) — surfaced as a
/// warn-only [`consts::SCHEDULE_TIMEZONE_AMBIGUOUS_CONDITION`] recommending an
/// explicit `spec.schedule.timezone`; `None` clears it back to the resolved state.
/// Returns the `conditions` + `observedGeneration` to merge into a status patch.
/// Existing conditions are preserved by [`io::set_ready`]'s upsert.
fn schedule_ready_status(
    schedule: &SnapshotSchedule,
    ambiguity: Option<&TimezoneAmbiguity>,
    blocked: Option<&UnreadableRun>,
    fanout_cap: Option<&[FanoutCapSkip]>,
) -> (serde_json::Value, i64) {
    use crate::consts::{
        BLOCKED_ON_UNREADABLE_RUN_REASON, FANOUT_TOO_LARGE_REASON, FANOUT_WITHIN_CAP_REASON,
        SCHEDULE_FANOUT_CAPPED_CONDITION, SCHEDULE_RUNNABLE_CONDITION,
        SCHEDULE_TIMEZONE_AMBIGUOUS_CONDITION, SCHEDULE_TIMEZONE_AMBIGUOUS_REASON,
        SCHEDULE_TIMEZONE_RESOLVED_REASON,
    };
    let existing = schedule
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    let generation = schedule.metadata.generation.unwrap_or(0);
    let (reason, message) = if schedule.spec.schedule.suspend {
        ("Suspended", "the schedule is suspended")
    } else {
        ("Scheduled", "the schedule is reconciled and active")
    };
    let conditions = io::set_ready(
        &existing,
        Some(generation),
        io::ReadyOutcome::Ready,
        reason,
        message,
    );
    // Upsert the timezone-ambiguity condition either way so it clears once the
    // disagreement is resolved (order-stable, transition-time-preserving).
    let tz_message = match ambiguity {
        Some(a) => format!(
            "matched policies' repository scheduleDefaults.timezone disagree ({}); \
             defaulting to UTC — set spec.schedule.timezone explicitly to pick a zone",
            a.candidates.join(", ")
        ),
        None => "schedule timezone resolved without ambiguity".to_string(),
    };
    let conditions = io::upsert_condition(
        &conditions,
        SCHEDULE_TIMEZONE_AMBIGUOUS_CONDITION,
        ambiguity.is_some(),
        if ambiguity.is_some() {
            SCHEDULE_TIMEZONE_AMBIGUOUS_REASON
        } else {
            SCHEDULE_TIMEZONE_RESOLVED_REASON
        },
        &tz_message,
        Some(generation),
    );
    // Upsert the runnable gate either way, so it CLEARS the moment the schedule
    // can fire again (the blocking Snapshot was deleted, or this build learned
    // the phase). A registered structural gate that could only ever be set would
    // be worse than none — doctor would report a resolved outage forever.
    let (runnable_reason, runnable_message) = match blocked {
        Some(b) => (
            BLOCKED_ON_UNREADABLE_RUN_REASON,
            format!(
                "Snapshot `{}` holds this schedule's concurrency gate at phase `{}`, which this \
                 kopiur build does not recognize (most likely a newer operator wrote it). This \
                 build can never observe that run finish, so under `concurrencyPolicy: Forbid` NO \
                 FURTHER BACKUPS WILL RUN for this schedule. Fix: Finish the operator upgrade, or \
                 delete Snapshot `{}` if the run is genuinely over, to release the gate.",
                b.snapshot, b.phase, b.snapshot
            ),
        ),
        None => (
            "Runnable",
            "the schedule's concurrency gate is clear".to_string(),
        ),
    };
    let conditions = io::upsert_condition(
        &conditions,
        SCHEDULE_RUNNABLE_CONDITION,
        blocked.is_none(),
        runnable_reason,
        &runnable_message,
        Some(generation),
    );
    // Fan-out cap condition (#368): asserted (both polarities, so a fixed
    // recipe self-clears) ONLY on fire passes — `None` on wait/hold passes
    // leaves the recorded value untouched, so the condition persists between
    // slots instead of flapping set→cleared each requeue.
    let conditions = match fanout_cap {
        None => conditions,
        Some([]) => io::upsert_condition(
            &conditions,
            SCHEDULE_FANOUT_CAPPED_CONDITION,
            false,
            FANOUT_WITHIN_CAP_REASON,
            "every fired slot's fan-out stayed within the cap",
            Some(generation),
        ),
        Some(skips) => {
            let detail: Vec<String> = skips
                .iter()
                .map(|sk| {
                    format!(
                        "policy `{}`: {} member(s) x {} repositorie(s) = {} children",
                        sk.policy,
                        sk.members,
                        sk.repos,
                        sk.members.saturating_mul(sk.repos)
                    )
                })
                .collect();
            io::upsert_condition(
                &conditions,
                SCHEDULE_FANOUT_CAPPED_CONDITION,
                true,
                FANOUT_TOO_LARGE_REASON,
                &format!(
                    "this slot was SKIPPED for {}: the source-members x repositories cross \
                     product exceeds the fan-out cap ({FANOUT_CAP} children per slot). No \
                     backups were minted for the listed polic(ies) and none will be until the \
                     pvcSelector is narrowed or spec.repositories shrunk.",
                    detail.join("; ")
                ),
                Some(generation),
            )
        }
    };
    (serde_json::json!(conditions), generation)
}

/// Whether `status.conditions` already records the blocked-on-unreadable-run
/// gate. The transition guard for its Warning Event: this branch re-runs on
/// every hold requeue, and an Event per pass would bury the cluster's event log.
fn recorded_blocked_on_unreadable(schedule: &SnapshotSchedule) -> bool {
    schedule
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or_default()
        .iter()
        .find(|c| c.type_ == crate::consts::SCHEDULE_RUNNABLE_CONDITION)
        .is_some_and(|c| {
            c.status == "False" && c.reason == crate::consts::BLOCKED_ON_UNREADABLE_RUN_REASON
        })
}

/// Reconcile a `SnapshotSchedule`.
#[tracing::instrument(skip(schedule, ctx), fields(kind = "SnapshotSchedule", namespace = %schedule.namespace().unwrap_or_default(), name = %schedule.name_any()))]
pub async fn reconcile(schedule: Arc<SnapshotSchedule>, ctx: Arc<Context>) -> Result<Action> {
    // A dispatched reconcile is proof the SnapshotSchedule reflector synced (the
    // applier gates on `store.wait_until_ready()`), so the breaker's owner lookup
    // (`schedule_owner_lookup`) can trust the store. See `Context::mark_schedule_synced`.
    ctx.mark_schedule_synced();
    let start = std::time::Instant::now();
    let result = reconcile_inner(&schedule, &ctx).await;
    ctx.metrics
        .record_reconcile("SnapshotSchedule", start.elapsed().as_secs_f64());
    result
}

async fn reconcile_inner(schedule: &SnapshotSchedule, ctx: &Context) -> Result<Action> {
    // Defensive re-validation (one validator, two callers — SKILL hard-rule 4).
    let errs = validate::validate_backup_schedule(&schedule.spec);
    if let Some(first) = errs.into_iter().next() {
        return Err(Error::Validation(first.to_string()));
    }

    let namespace = schedule
        .namespace()
        .ok_or_else(|| Error::Invariant("SnapshotSchedule has no namespace".into()))?;
    let sched_name = schedule.name_any();
    let api: Api<SnapshotSchedule> = Api::namespaced(ctx.client.clone(), &namespace);

    // ONE `SCHEDULE_LABEL` population per reconcile (#382 M1): the same slice
    // serves the failed-history prune, the `onScheduleDelete` propagation, and
    // the concurrency gate further down — these were three byte-identical LISTs.
    // Prune and propagation stay best-effort (a transient list/delete error must
    // NOT block firing the due backup; the next reconcile retries), but the
    // concurrency gate below keeps failing CLOSED: when this list errored, the
    // gate read propagates that error exactly as its own list did before.
    let snap_api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), &namespace);
    let children = schedule_children(ctx, &namespace, &sched_name).await;
    match &children {
        Ok(items) => {
            // Bound failure history: prune this schedule's oldest `Failed`
            // Snapshots beyond `failedJobsHistoryLimit` (GFS retention only
            // prunes successes), so a persistently-failing precondition or
            // backend can't accumulate `Failed` CRs without limit.
            if let Err(e) = prune_failed_history(
                &snap_api,
                &sched_name,
                items,
                schedule.spec.failed_jobs_history_limit,
            )
            .await
            {
                tracing::warn!(schedule = %sched_name, error = %e, "failed-history prune errored; continuing to schedule");
            }
            // Propagate a `spec.deletion.onScheduleDelete` edit to existing
            // produced children whose stamped cascade value has drifted (so an
            // edit to Delete actually cascades already-created Snapshots, not
            // just future ones).
            let desired_cascade = kopiur_api::snapshot_schedule::effective_on_schedule_delete(
                schedule.spec.deletion.as_ref(),
            );
            if let Err(e) =
                propagate_cascade_stamp(&snap_api, &sched_name, items, desired_cascade).await
            {
                tracing::warn!(schedule = %sched_name, error = %e, "onScheduleDelete propagation errored; continuing to schedule");
            }
        }
        Err(e) => {
            tracing::warn!(schedule = %sched_name, error = %e, "listing Snapshot children failed; failed-history prune and onScheduleDelete propagation skipped this pass");
        }
    }

    let seed = schedule.uid().unwrap_or_else(|| schedule.name_any());
    let now = Utc::now();
    let jitter_window = schedule
        .spec
        .schedule
        .jitter
        .as_deref()
        .and_then(parse_go_duration);
    // Effective cron timezone resolution for this reconcile. When the schedule sets
    // its own `spec.schedule.timezone`, that wins with no lookups. Otherwise inherit
    // from the target policies' repository `scheduleDefaults.timezone` (agree-or-UTC;
    // a disagreement among selector matches degrades to UTC + a status condition). A
    // referent GET/list failure (or a missing policy/repo) yields `Degraded` — which
    // must NOT invalidate an established pin (a transient apiserver blip would
    // otherwise flap the pinned slot to UTC and back); it only self-heals a first pin
    // to UTC. See `resolve_pinned_slot_tz` / `first_pin_tz`.
    let resolution = resolve_effective_timezone(ctx, schedule, &namespace).await;

    // The previously-pinned slot (status.nextSchedule) is the one that may now be
    // due. If absent (first reconcile), compute the upcoming slot from now and
    // pin it without firing (GitOps-friendly: runOnCreate defaults false).
    let pinned = schedule
        .status
        .as_ref()
        .and_then(|s| s.next_schedule.as_ref());
    let pinned_slot = pinned
        .and_then(|r| r.at.as_deref())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));
    // The timezone this slot was pinned in. Absent on legacy pins (written before the
    // field existed) → treated as "unchanged" so an upgrade never churns the pin.
    let pinned_tz = pinned.and_then(|r| r.timezone.clone());

    if let Some(slot) = pinned_slot {
        // Effective zone + whether the pin is stale, honoring Degraded semantics:
        // `Resolved` invalidates iff the recorded zone actually changed; `Degraded`
        // keeps the pin's own zone and never invalidates (no flap on a referent blip).
        let (tz, tz_ambiguity, needs_recompute) =
            resolve_pinned_slot_tz(pinned_tz.as_deref(), &resolution);
        // If the effective timezone changed since this slot was pinned (a
        // `spec.schedule.timezone` edit or a repo `scheduleDefaults` change), the
        // pinned wall-clock instant is stale. Recompute deterministically via the
        // existing `next_fire` (croner + deterministic jitter — NO new randomness)
        // and re-pin in the new zone without firing; the requeue re-enters and the
        // freshly-pinned slot fires when due. The equal case falls through untouched.
        if needs_recompute {
            let next = next_fire(&schedule.spec.schedule.cron, jitter_window, &seed, now, tz)?;
            // Re-pinning for a timezone change CLEARS the runnable gate, and that
            // is correct rather than merely convenient: this path abandons the
            // stale slot and pins a fresh one in the future, so at this instant
            // there is no due slot for anything to hold shut. If the blocker is
            // still there when the new slot comes due, the concurrency path below
            // re-sets the gate (and re-Events) then.
            let (conditions, generation) =
                schedule_ready_status(schedule, tz_ambiguity.as_ref(), None, None);
            io::patch_status(
                &api,
                &sched_name,
                serde_json::json!({
                    "nextSchedule": { "at": next.to_rfc3339(), "timezone": tz.name() },
                    "observedGeneration": generation,
                    "conditions": conditions,
                }),
            )
            .await?;
            tracing::info!(
                schedule = %sched_name, from = ?pinned_tz, to = %tz.name(),
                "effective timezone changed; recomputed the pinned slot"
            );
            let until = (next - now).to_std().unwrap_or(StdDuration::from_secs(60));
            return Ok(Action::requeue(until.max(StdDuration::from_secs(1))));
        }
        // Is a run currently active (an unfinished Snapshot owned by this
        // schedule)? Classified from the children slice fetched once above; a
        // list failure fails CLOSED here (propagates), never "no active runs".
        let runs = match children {
            Ok(ref items) => classify_active_runs(items),
            Err(e) => return Err(e),
        };
        let disposition = slot_disposition(&schedule.spec.schedule, slot, now, runs.active);
        // The blocker only MATTERS when it is actually holding a due slot shut:
        // under `concurrencyPolicy: Allow` an unreadable run does not block, and
        // a suspended or not-yet-due schedule is not being held by anything.
        // Computed once here so every status write below either sets or CLEARS
        // the `ScheduleRunnable` gate from the same fact.
        let blocker = runs.unreadable.as_ref().filter(|_| {
            disposition == SlotDisposition::Wait
                && !schedule.spec.schedule.suspend
                && should_fire_now(slot, now)
        });
        // A slot expired past `startingDeadlineSeconds` must re-pin forward, or the
        // wait branch below computes `(slot - now)` for a past instant and requeues
        // at the 1s floor forever with the pin stuck on the expired slot. Skip it
        // (CronJob missed-slot semantics), record the skip as a Normal event, and
        // pin the next upcoming slot.
        if disposition == SlotDisposition::SkipExpired {
            let next = next_fire(&schedule.spec.schedule.cron, jitter_window, &seed, now, tz)?;
            let (conditions, generation) =
                schedule_ready_status(schedule, tz_ambiguity.as_ref(), blocker, None);
            io::patch_status(
                &api,
                &sched_name,
                serde_json::json!({
                    "nextSchedule": { "at": next.to_rfc3339(), "timezone": tz.name() },
                    "observedGeneration": generation,
                    "conditions": conditions,
                }),
            )
            .await?;
            io::publish_normal_event(
                ctx,
                schedule,
                "MissedSchedule",
                "SkipExpiredSlot",
                &format!(
                    "slot {} expired past startingDeadlineSeconds ({}s) and was skipped; next slot pinned at {}",
                    slot.to_rfc3339(),
                    schedule.spec.schedule.starting_deadline_seconds.unwrap_or(0),
                    next.to_rfc3339(),
                ),
            )
            .await;
            tracing::info!(
                schedule = %sched_name, slot = %slot.to_rfc3339(), next = %next.to_rfc3339(),
                "skipped an expired slot (startingDeadlineSeconds); re-pinned forward"
            );
            let until = (next - now).to_std().unwrap_or(StdDuration::from_secs(60));
            return Ok(Action::requeue(until.max(StdDuration::from_secs(1))));
        }
        if disposition == SlotDisposition::Fire {
            // Fire one Snapshot per resolved policy (single policyRef, or each
            // policySelector match — ADR-0005 §10). The single-ref form keeps the
            // slot-stamped name for lastSchedule.snapshotRef.
            let outcome = fire_for_targets(ctx, schedule, &namespace, slot).await?;
            let snapshot_ref = schedule
                .spec
                .policy_ref
                .as_ref()
                .filter(|_| !outcome.fanned_out)
                .map(|_| scheduled_backup_name(&sched_name, slot));
            let next = next_fire(&schedule.spec.schedule.cron, jitter_window, &seed, now, tz)?;
            let (conditions, generation) = schedule_ready_status(
                schedule,
                tz_ambiguity.as_ref(),
                blocker,
                Some(&outcome.cap_skipped),
            );
            io::patch_status(
                &api,
                &sched_name,
                serde_json::json!({
                    "lastSchedule": { "at": slot.to_rfc3339(), "snapshotRef": snapshot_ref.map(|n| serde_json::json!({ "name": n })) },
                    "nextSchedule": { "at": next.to_rfc3339(), "timezone": tz.name() },
                    "consecutiveFailures": 0,
                    "observedGeneration": generation,
                    "conditions": conditions,
                }),
            )
            .await?;
            let until = (next - now).to_std().unwrap_or(StdDuration::from_secs(60));
            return Ok(Action::requeue(until.max(StdDuration::from_secs(1))));
        }
        // A DUE slot held shut by a run this build cannot read. Unlike every other
        // `Wait`, this one never resolves on its own: `classify_active_runs` fails
        // closed on an unreadable phase (correctly — it may be a live run), but this
        // build can never see that phase become terminal, so under the default
        // `concurrencyPolicy: Forbid` the schedule stops firing PERMANENTLY while
        // every object involved still looks healthy. That is #359's shape one kind
        // removed, so it is surfaced on the SnapshotSchedule itself: a registered
        // structural gate (`ScheduleRunnable=False`) plus a one-shot Warning Event.
        if let Some(blocked) = blocker {
            let (conditions, generation) =
                schedule_ready_status(schedule, tz_ambiguity.as_ref(), Some(blocked), None);
            io::patch_status(
                &api,
                &sched_name,
                serde_json::json!({
                    "observedGeneration": generation,
                    "conditions": conditions,
                }),
            )
            .await?;
            // Transition-guarded: this branch re-runs on every hold requeue, and
            // one Event per pass would bury the namespace's event log.
            if !recorded_blocked_on_unreadable(schedule) {
                io::publish_warning_event(
                    ctx,
                    schedule,
                    crate::consts::BLOCKED_ON_UNREADABLE_RUN_REASON,
                    "FinishOperatorUpgrade",
                    &format!(
                        "no further backups will run: Snapshot `{}` holds the concurrency gate \
                         at phase `{}`, which this kopiur build does not recognize (a newer \
                         operator most likely wrote it). Finish the operator upgrade, or delete \
                         that Snapshot if its run is genuinely over.",
                        blocked.snapshot, blocked.phase
                    ),
                )
                .await;
            }
            tracing::warn!(
                namespace = %namespace,
                schedule = %sched_name,
                blocking_snapshot = %blocked.snapshot,
                phase = %blocked.phase,
                "SnapshotSchedule is blocked: a previous run sits at a phase this operator \
                 build does not recognize, so the concurrency gate can never clear here"
            );
            return Ok(Action::requeue(UNREADABLE_RUN_HOLD_REQUEUE));
        }

        // Slot not yet due: wait until it is. The ambiguity condition is otherwise
        // only rewritten on a status-patching path, so a resolved (or newly-arisen)
        // ambiguity could linger until the next fire. When resolution succeeded and
        // the freshly-computed state differs from what's recorded, patch just the
        // conditions; the equality guard keeps steady state patch-free, and a Degraded
        // pass is skipped entirely (it asserts nothing about ambiguity).
        // The runnable gate rides along for the same reason: `blocker` is `None`
        // here, so a schedule that was blocked and is now free clears the gate on
        // the first pass after the blocking Snapshot goes away.
        if matches!(resolution, TimezoneResolution::Resolved { .. })
            && (tz_ambiguity.is_some() != recorded_tz_ambiguous(schedule)
                || recorded_blocked_on_unreadable(schedule))
        {
            let (conditions, generation) =
                schedule_ready_status(schedule, tz_ambiguity.as_ref(), blocker, None);
            io::patch_status(
                &api,
                &sched_name,
                serde_json::json!({
                    "observedGeneration": generation,
                    "conditions": conditions,
                }),
            )
            .await?;
        }
        let until = (slot - now).to_std().unwrap_or(StdDuration::from_secs(1));
        return Ok(Action::requeue(until.max(StdDuration::from_secs(1))));
    }

    // First reconcile (nextSchedule not yet pinned). Choose the zone to pin: the
    // resolved zone, or UTC when Degraded (self-heals — once referents recover, the
    // pinned-slot branch recomputes into the inherited zone exactly once).
    let (tz, tz_ambiguity) = first_pin_tz(&resolution);
    let next = next_fire(&schedule.spec.schedule.cron, jitter_window, &seed, now, tz)?;

    // Honor `runOnCreate`: fire one backup immediately instead of waiting for the
    // first cron slot. The run is anchored to the schedule's creation time (not
    // `now`) so its deterministic name is stable across retries — if the status
    // patch below fails and we re-enter this branch, the server-side apply
    // converges on the same Snapshot rather than creating a duplicate.
    let already_ran = schedule
        .status
        .as_ref()
        .and_then(|s| s.last_schedule.as_ref())
        .is_some();
    if should_run_on_create(&schedule.spec.schedule, already_ran) {
        // metadata.creationTimestamp is a k8s-openapi `Time` wrapping a jiff
        // `Timestamp`; convert via unix seconds to chrono (matches snapshot_policy).
        let anchor = schedule
            .creation_timestamp()
            .and_then(|t| DateTime::<Utc>::from_timestamp(t.0.as_second(), 0))
            .unwrap_or(now);
        let outcome = fire_for_targets(ctx, schedule, &namespace, anchor).await?;
        let snapshot_ref = schedule
            .spec
            .policy_ref
            .as_ref()
            .filter(|_| !outcome.fanned_out)
            .map(|_| scheduled_backup_name(&sched_name, anchor));
        // First reconcile: no prior run of this schedule exists, so no blocker.
        let (conditions, generation) = schedule_ready_status(
            schedule,
            tz_ambiguity.as_ref(),
            None,
            Some(&outcome.cap_skipped),
        );
        io::patch_status(
            &api,
            &sched_name,
            serde_json::json!({
                "lastSchedule": { "at": anchor.to_rfc3339(), "snapshotRef": snapshot_ref.map(|n| serde_json::json!({ "name": n })) },
                "nextSchedule": { "at": next.to_rfc3339(), "timezone": tz.name() },
                "consecutiveFailures": 0,
                "observedGeneration": generation,
                "conditions": conditions,
            }),
        )
        .await?;
        let until = (next - now).to_std().unwrap_or(StdDuration::from_secs(60));
        return Ok(Action::requeue(until.max(StdDuration::from_secs(1))));
    }

    // No runOnCreate: pin the next slot without firing (GitOps-friendly default).
    let (conditions, generation) =
        schedule_ready_status(schedule, tz_ambiguity.as_ref(), None, None);
    io::patch_status(
        &api,
        &sched_name,
        serde_json::json!({
            "nextSchedule": { "at": next.to_rfc3339(), "timezone": tz.name() },
            "observedGeneration": generation,
            "conditions": conditions,
        }),
    )
    .await?;
    let until = (next - now).to_std().unwrap_or(StdDuration::from_secs(60));
    Ok(Action::requeue(until.max(StdDuration::from_secs(1))))
}

/// A deterministic, slot-stamped Snapshot name so the same slot is idempotent
/// across reconciles/replicas (`<schedule>-<YYYYmmddHHMMSS>`).
fn scheduled_backup_name(schedule: &str, slot: DateTime<Utc>) -> String {
    format!("{schedule}-{}", slot.format("%Y%m%d%H%M%S"))
}

/// A run of this schedule that holds the concurrency gate open on a phase string
/// THIS build cannot interpret — i.e. version skew, written by a newer kopiur.
///
/// Named separately from the plain "a run is active" bool because the two have
/// opposite futures: an ordinary active run reaches a terminal phase and
/// releases the gate, whereas this one never can under this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableRun {
    /// `metadata.name` of the blocking `Snapshot`.
    pub snapshot: String,
    /// Its `status.phase` verbatim, so the operator sees the actual string.
    pub phase: String,
}

/// The concurrency gate's answer: whether any unfinished run of this schedule
/// exists, plus — when the blocker is a phase this build cannot read — which one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActiveRuns {
    /// An unfinished `Snapshot` owned by this schedule exists.
    pub active: bool,
    /// The first blocker whose phase is `SnapshotPhase::Unknown`, if any.
    pub unreadable: Option<UnreadableRun>,
}

/// **Pure.** Classify this schedule's `Snapshot` children into [`ActiveRuns`].
/// Split from the `Api::list` so the concurrency gate — including the
/// version-skew blocker it must surface — is unit-tested without a cluster.
pub fn classify_active_runs(items: &[Snapshot]) -> ActiveRuns {
    use kopiur_api::SnapshotPhase;
    let mut out = ActiveRuns::default();
    for b in items {
        if b.metadata.deletion_timestamp.is_some() {
            continue;
        }
        // Exhaustive: "unfinished" is the complement of the terminal set here,
        // so a new phase must state which side of the schedule's concurrency
        // gate it falls on before it compiles.
        let unfinished = match b.status.as_ref().and_then(|s| s.phase.as_ref()) {
            None | Some(SnapshotPhase::Pending | SnapshotPhase::Running) => true,
            Some(
                SnapshotPhase::Succeeded
                | SnapshotPhase::Failed
                | SnapshotPhase::Deleting
                | SnapshotPhase::Discovered
                | SnapshotPhase::Unchanged,
            ) => false,
            // Fail CLOSED: an unreadable phase may well be an in-flight run
            // written by a newer operator, and starting a second run for the
            // same schedule concurrently is the outcome this gate exists to
            // prevent. But failing closed here can never clear on its own — see
            // `unreadable` and the `ScheduleRunnable` gate that surfaces it.
            Some(SnapshotPhase::Unknown(raw)) => {
                if out.unreadable.is_none() {
                    out.unreadable = Some(UnreadableRun {
                        snapshot: b.name_any(),
                        phase: raw.clone(),
                    });
                }
                true
            }
        };
        out.active |= unfinished;
    }
    out
}

/// This schedule's produced `Snapshot` children (the `SCHEDULE_LABEL`
/// population), fetched ONCE per reconcile and shared by the failed-history
/// prune, the `onScheduleDelete` propagation, and the concurrency gate (#382
/// M1 — these were previously three byte-identical LISTs per reconcile).
///
/// Served from the Snapshot reflector store when synced, live LIST otherwise
/// (#382 M3 via [`io::snapshot_children`] — namespace AND label filtered, C4).
/// The Forbid concurrency gate keeps failing CLOSED: a cold store falls
/// through to the live LIST, and a live-LIST error propagates to the gate
/// exactly as its own LIST error did before — an unavailable population is
/// never read as "no active runs".
async fn schedule_children(
    ctx: &Context,
    namespace: &str,
    schedule: &str,
) -> Result<Vec<Snapshot>> {
    io::snapshot_children(ctx, namespace, crate::consts::SCHEDULE_LABEL, schedule).await
}

/// The terminal time used to order Failed snapshots for pruning: `status.timing.endTime`
/// when present, else `metadata.creationTimestamp`. `None` only when neither is set.
fn snapshot_terminal_time(s: &Snapshot) -> Option<DateTime<Utc>> {
    s.status
        .as_ref()
        .and_then(|st| st.timing.as_ref())
        .and_then(|t| t.end_time.as_deref())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            s.creation_timestamp()
                .and_then(|t| DateTime::<Utc>::from_timestamp(t.0.as_second(), 0))
        })
}

/// **Pure.** Names of the `Failed` snapshots to delete so at most `limit` (the
/// newest, by terminal time) are retained PER REPOSITORY. Skips snapshots
/// already terminating. Mirrors `snapshot_policy::backups_to_delete` (GFS
/// retention) but for failures.
///
/// **Bucketing is by REPOSITORY ONLY** (the mint-time `spec.repository` pin's
/// [`repo_key`]; unpinned rows share one bucket), deliberately NOT by source:
/// single-repo and `pvcSelector` populations are all unpinned, so they land in
/// the one "" bucket — today's flat behavior byte-identical (bucketing by
/// source here would have 20×'d the retained failure rows for a 20-PVC
/// fan-out). What the repo dimension prevents is cross-repo eviction: with two
/// repositories, an outage of repo B fails every B child each slot, and a flat
/// bound would evict repo A's (rarer, still-diagnostic) failure records while
/// keeping only B's flood.
///
/// **Data-safety:** never prunes a `Failed` snapshot that owns a kopia snapshot
/// (`status.snapshot` set) — a backup can end `Failed` *after* its kopia snapshot
/// was created (e.g. an `afterSnapshot` hook aborts), and deleting that CR under the
/// default `Delete` policy would run `kopia snapshot delete` and destroy a real,
/// recoverable backup. Those CRs are kept; only artifact-less failures (preflight,
/// pre-snapshot errors) are history-bounded here.
pub(crate) fn failed_snapshots_to_prune(snapshots: &[Snapshot], limit: u32) -> Vec<String> {
    use kopiur_api::SnapshotPhase;
    let failed = snapshots.iter().filter(|s| {
        let st = s.status.as_ref();
        // Exhaustive, not `== Failed`: this set's members get DELETED, so
        // every phase must say out loud whether it belongs in the
        // failure-history bound rather than inheriting an answer.
        let is_failed_history = st.and_then(|s| s.phase.as_ref()).is_some_and(|p| match p {
            SnapshotPhase::Failed => true,
            SnapshotPhase::Pending
            | SnapshotPhase::Running
            | SnapshotPhase::Succeeded
            | SnapshotPhase::Deleting
            | SnapshotPhase::Discovered
            | SnapshotPhase::Unchanged => false,
            // Never delete a CR whose phase this build cannot read.
            SnapshotPhase::Unknown(_) => false,
        });
        is_failed_history
            && s.metadata.deletion_timestamp.is_none()
            // Never auto-delete a Failed snapshot that produced a kopia snapshot.
            && st.and_then(|s| s.snapshot.as_ref()).is_none()
    });
    let mut buckets: BTreeMap<String, Vec<&Snapshot>> = BTreeMap::new();
    for s in failed {
        let key = s
            .spec
            .repository
            .as_ref()
            .map(|r| repo_key(r, s.namespace().as_deref().unwrap_or_default()))
            .unwrap_or_default();
        buckets.entry(key).or_default().push(s);
    }
    buckets
        .into_values()
        .flat_map(|mut rows| {
            // Newest first; an unknown terminal time (`None`) sorts last
            // (treated as oldest) → pruned first.
            rows.sort_by_key(|s| std::cmp::Reverse(snapshot_terminal_time(s)));
            rows.into_iter()
                .skip(limit as usize)
                .filter_map(|s| s.metadata.name.clone())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Enforce `failedJobsHistoryLimit`: prune the schedule's oldest `Failed` Snapshots
/// beyond the limit. Consumes the shared per-reconcile children slice (#382 M1)
/// and the GFS-prune delete idiom (delete the CR → its finalizer +
/// `deletionPolicy` handle any kopia cleanup).
async fn prune_failed_history(
    api: &Api<Snapshot>,
    schedule: &str,
    children: &[Snapshot],
    limit: Option<u32>,
) -> Result<()> {
    let limit = kopiur_api::consts::effective_failed_jobs_history_limit(limit);
    for name in failed_snapshots_to_prune(children, limit) {
        // #382 C2: `children` may be store-derived — trust the cache to SELECT,
        // verify live before DESTROYING. A row that vanished or started
        // terminating since the reflector snapshot is skipped.
        if !io::confirm_row_live(api, &name).await? {
            continue;
        }
        // Stamp `pruned-by: failed-history` THEN delete, so the finalizer treats
        // this as an operator prune (bypassing the mass-deletion breaker), never
        // an external deletion. `failed_snapshots_to_prune` already excludes
        // terminating CRs, so there is no stamp-only partition here.
        io::annotate_then_delete_snapshot(api, &name, PrunedBy::FailedHistory).await?;
        tracing::info!(schedule = %schedule, snapshot = %name, "pruned Failed Snapshot (failedJobsHistoryLimit)");
    }
    Ok(())
}

/// Propagate a `spec.deletion.onScheduleDelete` edit to this schedule's existing
/// produced `Snapshot` children (labelled `SCHEDULE_LABEL`) whose stamped value
/// has drifted from `desired` ([`children_needing_cascade_stamp`]). One targeted
/// merge-patch per child under the controller field manager. Consumes the shared
/// per-reconcile children slice (#382 M1). Best-effort exactly like
/// [`prune_failed_history`]: a per-child error is logged and the reconcile
/// continues — propagation must never block firing the due backup.
async fn propagate_cascade_stamp(
    api: &Api<Snapshot>,
    schedule: &str,
    children: &[Snapshot],
    desired: ScheduleDeletePolicy,
) -> Result<()> {
    let value = serde_json::to_value(desired).unwrap_or(serde_json::Value::Null);
    for name in children_needing_cascade_stamp(children, desired) {
        let patch = serde_json::json!({ "spec": { "onScheduleDelete": value } });
        match api
            .patch(
                &name,
                &PatchParams::apply(io::FIELD_MANAGER),
                &Patch::Merge(&patch),
            )
            .await
        {
            Ok(_) => {
                tracing::info!(schedule = %schedule, snapshot = %name, ?desired, "propagated onScheduleDelete to child")
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}
            Err(e) => {
                tracing::warn!(schedule = %schedule, snapshot = %name, error = %e, "propagating onScheduleDelete to child failed; continuing")
            }
        }
    }
    Ok(())
}

/// The `Snapshot` at slot name `name` in `namespace`, for the
/// fire-into-a-terminating-object guard ([`slot_fire_blocked_by_terminating`]).
/// Prefers the shared `Snapshot` reflector store when it is populated AND synced
/// (no per-fire GET); falls back to a live `get_opt` when the store is unset or
/// not yet synced (a cold cache must never be read as "no twin").
async fn slot_twin(ctx: &Context, namespace: &str, name: &str) -> Result<Option<Arc<Snapshot>>> {
    use std::sync::atomic::Ordering;
    if let Some(store) = ctx.snapshot_store.get()
        && ctx.snapshot_synced.load(Ordering::Acquire)
    {
        return Ok(store.get(&ObjectRef::<Snapshot>::new(name).within(namespace)));
    }
    let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), namespace);
    Ok(api.get_opt(name).await?.map(Arc::new))
}

/// Resolve the set of `SnapshotPolicy` targets a fire should create a `Snapshot`
/// for (ADR-0005 §10). With `policyRef` it's the single named policy. With
/// `policySelector` it lists `SnapshotPolicy`s in the schedule's namespace and
/// returns each matching the selector (skipping suspended policies — §14(e)). The
/// XOR is webhook-enforced and re-validated in `reconcile_inner`; here a schedule
/// with neither yields an empty set (no fire).
async fn target_policy_refs(
    ctx: &Context,
    schedule: &SnapshotSchedule,
    namespace: &str,
) -> Result<Vec<PolicyRef>> {
    if let Some(pref) = &schedule.spec.policy_ref {
        return Ok(vec![pref.clone()]);
    }
    let Some(selector) = &schedule.spec.policy_selector else {
        return Ok(Vec::new());
    };
    // Fan-out: read SnapshotPolicies in the schedule's namespace and filter by the
    // selector. (The schedule fires policies in its own namespace; a policyRef may
    // still cross namespaces, but the selector form is namespace-local by design.)
    // Prefer the shared SnapshotPolicy reflector store when published AND synced
    // (#382 M2) — this runs twice per reconcile (timezone default + fire loop),
    // so serving it in-process removes two LISTs. A cold store falls back to the
    // live LIST: an unset/unsynced store read as "no targets" would silently
    // skip firing (design rule R2 — live fallback, never deferral-as-empty).
    if let Some(store) = ctx.config_store.get()
        && io::read_from_store(
            true,
            ctx.config_synced.load(std::sync::atomic::Ordering::Acquire),
        )
    {
        let state = store.state();
        return Ok(select_policy_targets(
            state.iter().map(Arc::as_ref),
            namespace,
            selector,
        ));
    }
    let api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), namespace);
    let policies = api.list(&ListParams::default()).await?.items;
    Ok(select_policy_targets(policies.iter(), namespace, selector))
}

/// **Pure.** The `policySelector` fan-out target set: every policy **in
/// `namespace`** matching `selector`, skipping suspended policies (§14(e)),
/// sorted by name (parity with the apiserver's name-ordered LIST, so the fire
/// loop's per-policy child naming is order-stable however the set was read).
///
/// The namespace filter is INSIDE this kernel deliberately (audit C4): the
/// reflector store is install-scope-wide, so an in-process replacement for a
/// namespaced label-selector LIST must filter namespace AND label — a
/// label-only filter would merge a matching policy from ANOTHER namespace into
/// this schedule's fan-out.
fn select_policy_targets<'a>(
    policies: impl IntoIterator<Item = &'a SnapshotPolicy>,
    namespace: &str,
    selector: &k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector,
) -> Vec<PolicyRef> {
    let mut refs: Vec<PolicyRef> = policies
        .into_iter()
        .filter(|p| {
            p.metadata.namespace.as_deref() == Some(namespace)
                && !p.spec.suspend
                && policy_matches_selector(
                    p.metadata.labels.as_ref().unwrap_or(&BTreeMap::new()),
                    selector,
                )
        })
        .map(|p| PolicyRef {
            name: p.name_any(),
            namespace: None,
        })
        .collect();
    refs.sort_by(|a, b| a.name.cmp(&b.name));
    refs
}

/// Resolve the effective cron timezone for this reconcile (see
/// [`effective_timezone`] for the rule). When `spec.schedule.timezone` is set, that
/// wins with no lookups (`Resolved`, never degraded, never ambiguous). Otherwise it
/// GETs each target policy and resolves that policy's repository
/// `scheduleDefaults.timezone`, then applies the agree-or-UTC rule.
///
/// Returns [`TimezoneResolution::Degraded`] on any referent GET/list failure (or a
/// missing policy/repo) rather than a genuinely-resolved UTC: a missing or unreadable
/// referent must not wedge scheduling, and — critically — must not be mistaken for a
/// real UTC and used to invalidate an established non-UTC pin (see
/// [`resolve_pinned_slot_tz`]). The pin's own requeue plus the referent watch recover
/// once the referent returns.
///
/// Note an *empty* matched-policy set (a selector that matches nothing) is a genuine
/// `Resolved { tz: UTC }`, not `Degraded` — there was nothing to inherit, and pinning
/// UTC there is correct.
async fn resolve_effective_timezone(
    ctx: &Context,
    schedule: &SnapshotSchedule,
    namespace: &str,
) -> TimezoneResolution {
    if let Some(own) = schedule.spec.schedule.timezone.as_deref() {
        return TimezoneResolution::Resolved {
            tz: resolve_tz(Some(own)),
            ambiguity: None,
        };
    }
    // Same target set the fire path uses (single policyRef, or each selector match).
    let policy_refs = match target_policy_refs(ctx, schedule, namespace).await {
        Ok(refs) => refs,
        Err(e) => {
            tracing::debug!(error = %e, "listing target policies for timezone default failed; degrading (established pin preserved)");
            return TimezoneResolution::Degraded;
        }
    };
    let mut defaults: Vec<Option<String>> = Vec::with_capacity(policy_refs.len());
    for pref in &policy_refs {
        match policy_repo_timezone_default(ctx, pref, namespace).await {
            // One entry per repository of the policy (#368): a multi-repo
            // policy's members feed the same agree-or-UTC kernel below, so
            // within-policy disagreement is UTC + the ambiguity condition.
            Ok(tzs) => defaults.extend(tzs),
            Err(e) => {
                tracing::debug!(policy = %pref.name, error = %e, "resolving policy repository timezone default failed; degrading (established pin preserved)");
                return TimezoneResolution::Degraded;
            }
        }
    }
    // `own` is provably `None` here (the explicit-timezone case returned above).
    let (tz, ambiguity) = effective_timezone(None, &defaults);
    TimezoneResolution::Resolved { tz, ambiguity }
}

/// GET one target policy and resolve its repository/-ies'
/// `scheduleDefaults.timezone`, one entry PER REPOSITORY (#368 audit M9: a
/// multi-repo policy contributes each member's default, so the entries flow
/// through the SAME agree-or-UTC + ambiguity kernel
/// ([`effective_timezone`]) as cross-policy defaults do — all members agree ⇒
/// that value; disagreement ⇒ UTC + the existing `TimezoneDefaultAmbiguous`
/// warning condition, deterministically, with no new admission machinery).
/// The single-repo shape returns exactly one entry — behavior unchanged.
/// Honors `policyRef.namespace` for a cross-namespace ref; the policy's
/// repositories resolve in the policy's own namespace (matching how the policy
/// itself resolves them). An entry is `None` when that repository sets no
/// default.
async fn policy_repo_timezone_default(
    ctx: &Context,
    policy_ref: &PolicyRef,
    schedule_ns: &str,
) -> Result<Vec<Option<String>>> {
    let policy_ns = policy_ref.namespace.as_deref().unwrap_or(schedule_ns);
    // Store-backed point reads (#382 M2): policy + repository both come from
    // the fetch kernel — a miss/cold store is live-confirmed, so the error
    // shapes (and the caller's Degraded handling) are unchanged.
    let policy = io::fetch_policy(ctx, policy_ns, &policy_ref.name)
        .await?
        .ok_or_else(|| {
            Error::MissingDependency(format!("SnapshotPolicy {policy_ns}/{}", policy_ref.name))
        })?;
    let mut defaults = Vec::new();
    for rref in kopiur_api::repository_refs(&policy.spec) {
        let repo = io::resolve_repository_ref_cached(ctx, rref, policy_ns).await?;
        defaults.push(repo.schedule_defaults.and_then(|d| d.timezone));
    }
    Ok(defaults)
}

/// Build the `SnapshotSpec` for a scheduled backup of `policy_ref`. Pure so the
/// `defaultDeletionPolicy` inheritance (issue #238) is unit-tested without a
/// cluster.
///
/// `default_deletion_policy` is the referenced `SnapshotPolicy`'s
/// `spec.defaultDeletionPolicy`. Threading it here — rather than always emitting
/// `deletion_policy: None` — is what makes the recipe-wide default actually reach
/// the produced `Snapshot`: the mutating admission webhook only fills in its
/// origin-aware `Delete` default when this field is `None`, so a never-resolved
/// policy default silently became `Delete` regardless of what the recipe asked
/// for. `None` here preserves that safe origin-aware default exactly.
/// `on_schedule_delete` is the schedule's EFFECTIVE cascade policy
/// (`effective_on_schedule_delete(spec.deletion.as_ref())`, absent → `Retain`),
/// stamped EXPLICITLY onto every produced `Snapshot` so the deletion finalizer's
/// cascade guard reads a concrete value rather than inferring one — a schedule
/// edited to `Delete` after a run propagates to existing children via
/// [`children_needing_cascade_stamp`].
fn scheduled_backup_spec(
    policy_ref: &PolicyRef,
    default_deletion_policy: Option<DeletionPolicy>,
    on_schedule_delete: ScheduleDeletePolicy,
    source: Option<kopiur_api::SnapshotSourceRef>,
    repository: Option<RepositoryRef>,
) -> SnapshotSpec {
    SnapshotSpec {
        // The mint-time repository pin: stamped (NORMALIZED, see `mint_cells`)
        // only for a MULTI-repository policy fan-out child (#368) — single-repo
        // children stay unpinned so their wire shape is byte-identical to
        // pre-feature CRs.
        repository,
        // Which PVC this child covers, for a `pvcSelector` expansion. `None`
        // for the ordinary single-source policy (#346).
        source,
        policy_ref: Some(policy_ref.clone()),
        tags: None,
        failure_policy: None,
        deletion_policy: default_deletion_policy,
        // Always explicit for produced Snapshots (the cascade guard's input).
        on_schedule_delete: Some(on_schedule_delete),
        pin: false,
        // Scheduled backups never carry a templated description (out of
        // scope for M4 — description is per-invocation only).
        description: None,
    }
}

/// **Pure.** Names of a schedule's produced `Snapshot` children whose stamped
/// `spec.onScheduleDelete` must be re-stamped to `desired` (the schedule's
/// current effective cascade policy) after a `spec.deletion.onScheduleDelete`
/// edit. A child is selected iff it is NOT terminating AND its EFFECTIVE stamped
/// value differs from `desired`:
///
/// - stamped == `desired` → skip (already correct).
/// - stamped absent, `desired == Retain` → skip: an absent value already
///   resolves to `Retain` ([`effective_on_schedule_delete`]), so stamping it
///   would be pure status churn.
/// - stamped absent, `desired == Delete` → select: the child needs the explicit
///   `Delete` stamp so the cascade guard cascades.
/// - stamped differs (e.g. `Retain` vs. desired `Delete`) → select.
/// - terminating (deletionTimestamp set) → skip always: the child's finalizer is
///   already running on the value it captured; re-stamping it now is pointless
///   and races the delete.
pub fn children_needing_cascade_stamp(
    children: &[Snapshot],
    desired: ScheduleDeletePolicy,
) -> Vec<String> {
    children
        .iter()
        .filter(|c| c.metadata.deletion_timestamp.is_none())
        .filter(|c| {
            crate::snapshot::effective_on_schedule_delete(c.spec.on_schedule_delete) != desired
        })
        .filter_map(|c| c.metadata.name.clone())
        .collect()
}

/// **Pure.** Whether firing a slot must be SKIPPED because a `Snapshot` with the
/// target slot name already exists AND is terminating (`deletionTimestamp` set).
/// Re-firing would force-server-side-apply INTO that terminating object, silently
/// re-adopting/re-owning a CR whose finalizer is mid-cleanup
/// (fire-into-a-terminating-object). `None` (no such CR) ⇒ fire normally; the
/// next slot re-fires with a fresh name.
fn slot_fire_blocked_by_terminating(existing: Option<&Snapshot>) -> bool {
    existing.is_some_and(|s| s.metadata.deletion_timestamp.is_some())
}

/// **Pure.** Whether `policy` is safe to fire a scheduled `Snapshot` against —
/// it must not be mid-deletion. A `SnapshotPolicy` carrying a
/// `metadata.deletionTimestamp` is treated EXACTLY like an absent one by
/// [`policy_default_deletion_policy`]: firing into it would create a
/// `Snapshot` after the policy's own deletion-cascade finalizer
/// ([`crate::snapshot_policy::plan_policy_cascade`]) may already have LISTed
/// its children, stranding a child the cascade never accounts for.
fn policy_usable(policy: &SnapshotPolicy) -> bool {
    policy.metadata.deletion_timestamp.is_none()
}

/// GET the target policy and return its `spec.defaultDeletionPolicy` (issue #238).
/// Honors a cross-namespace `policyRef.namespace` exactly like
/// [`policy_repo_timezone_default`], and — like it — returns an **error** rather
/// than a value on a read failure, a missing policy, or a TERMINATING policy
/// ([`policy_usable`]), so the caller skips/retries the fire instead of firing
/// with a wrong default or into a policy that is cascading its own children away.
///
/// This must NOT degrade to `None` on a transient GET failure: `None` reaches the
/// mutating webhook, which stamps the origin default `Delete`, so an apiserver blip
/// against a policy whose `defaultDeletionPolicy` is `Retain`/`Orphan` would
/// silently downgrade the produced snapshot's retention to destructive `Delete`.
/// Deferring the fire (it re-fires idempotently on the next reconcile) is strictly
/// safer than persisting the wrong retention semantics. A genuine `None` here means
/// the policy exists but sets no default — then the webhook `Delete` default is
/// correct — never "we couldn't read it."
///
/// Known residual (not eliminated here): a TOCTOU window between this check and
/// the apply in [`create_scheduled_backup`] — the policy could start terminating
/// in between, and a late-fired child dangles, bounded by `failedJobsHistoryLimit`.
async fn policy_default_deletion_policy(
    ctx: &Context,
    policy_ref: &PolicyRef,
    schedule_ns: &str,
) -> Result<Option<DeletionPolicy>> {
    let policy_ns = policy_ref.namespace.as_deref().unwrap_or(schedule_ns);
    // Store-backed point read (#382 M2): a miss/cold store is live-confirmed
    // (`fetch_policy`), so the fail-the-fire error contract above is unchanged.
    let policy = io::fetch_policy(ctx, policy_ns, &policy_ref.name)
        .await?
        .ok_or_else(|| {
            Error::MissingDependency(format!("SnapshotPolicy {policy_ns}/{}", policy_ref.name))
        })?;
    if !policy_usable(&policy) {
        return Err(Error::MissingDependency(format!(
            "SnapshotPolicy {policy_ns}/{} is being deleted",
            policy_ref.name
        )));
    }
    Ok(policy.spec.default_deletion_policy)
}

/// Create a scheduled Snapshot CR for `policy_ref` (owner-ref to the schedule,
/// origin=scheduled). Server-side applied so re-firing the same slot converges
/// instead of erroring. `cell` carries the per-child name, source pin, and —
/// for a multi-repo fan-out child — the repository pin.
/// `default_deletion_policy` is resolved ONCE per policy by the caller
/// ([`fire_for_targets`], the audit-M10 GET hoist), not re-fetched per child.
async fn create_scheduled_backup(
    ctx: &Context,
    schedule: &SnapshotSchedule,
    namespace: &str,
    cell: &MintCell,
    policy_ref: &PolicyRef,
    default_deletion_policy: Option<DeletionPolicy>,
) -> Result<()> {
    let backup_name = cell.name.as_str();
    let owner = io::owner_ref_for(schedule, "SnapshotSchedule")?;
    let mut labels = std::collections::BTreeMap::new();
    labels.insert(
        ORIGIN_LABEL.to_string(),
        kopiur_api::Origin::Scheduled.label_value().to_string(),
    );
    labels.insert(
        crate::consts::SCHEDULE_LABEL.to_string(),
        schedule.name_any(),
    );
    labels.insert(
        crate::consts::CONFIG_LABEL.to_string(),
        policy_ref.name.clone(),
    );

    // The schedule's effective cascade policy, stamped explicitly onto the child.
    let on_schedule_delete = kopiur_api::snapshot_schedule::effective_on_schedule_delete(
        schedule.spec.deletion.as_ref(),
    );
    // Fire-into-a-terminating-object guard: if the target-slot Snapshot already
    // exists AND is terminating, the force server-side apply below would silently
    // re-adopt (re-own) the CR mid-cleanup. Skip this fire; the next slot re-fires
    // with a fresh name. (Prefer the reflector store; live GET fallback.)
    if slot_fire_blocked_by_terminating(slot_twin(ctx, namespace, backup_name).await?.as_deref()) {
        tracing::info!(schedule = %schedule.name_any(), backup = %backup_name, "target-slot Snapshot is terminating; skipping fire (next slot re-fires)");
        return Ok(());
    }

    let mut backup = Snapshot::new(
        backup_name,
        scheduled_backup_spec(
            policy_ref,
            default_deletion_policy,
            on_schedule_delete,
            cell.source.clone(),
            cell.repository.clone(),
        ),
    );
    backup.metadata = io::child_meta(backup_name, namespace, labels, Some(owner));

    let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), namespace);
    io::apply(&api, backup_name, &backup).await?;
    ctx.metrics
        .inc_schedule_backup_created(namespace, &schedule.name_any());
    tracing::info!(schedule = %schedule.name_any(), backup = %backup_name, policy = %policy_ref.name, deletion_policy = ?default_deletion_policy, "created scheduled Snapshot");
    Ok(())
}

pub(crate) use kopiur_api::expand::{MintCell, mint_cells};

/// Hard cap on one slot's members × repositories cross product. Above it the
/// slot is SKIPPED for that policy (Stalled-style condition + warning), never
/// partially minted: 400+ children per slot is a config mistake, and minting
/// them would bury the namespace in CRs and mover Jobs faster than anyone can
/// react.
const FANOUT_CAP: usize = 400;

/// **Pure.** The cap guard: whether crossing `members` source members with
/// `repos` repositories exceeds [`FANOUT_CAP`].
pub(crate) fn fanout_cap_exceeded(members: usize, repos: usize) -> bool {
    members.saturating_mul(repos) > FANOUT_CAP
}

/// A slot skipped by the fan-out cap, surfaced on the schedule's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FanoutCapSkip {
    pub policy: String,
    pub members: usize,
    pub repos: usize,
}

/// What [`fire_for_targets`] did with a slot.
pub(crate) struct FireOutcome {
    /// Whether ANY target minted children whose names differ from the bare
    /// slot-stamped reconstruction (pvcSelector and/or multi-repo fan-out) —
    /// the `status.lastSchedule.snapshotRef` suppression input.
    pub fanned_out: bool,
    /// Slots skipped because members × repos exceeded [`FANOUT_CAP`].
    pub cap_skipped: Vec<FanoutCapSkip>,
}

/// **Pure.** What to do with one policy's slot, given its expanded members:
/// mint nothing (empty selector match), skip on the fan-out cap, or mint the
/// exact [`mint_cells`] cross product. Extracted so the fire loop's IO stays a
/// straight dispatch and the decision is unit-testable.
enum SlotMintPlan {
    /// The `pvcSelector` matched nothing — warn, mint nothing.
    NothingMatched,
    /// members × repos exceeds [`FANOUT_CAP`] — skip the whole slot for this
    /// policy (a partial mint would silently protect some members-on-some-repos
    /// with no record of which) and surface the condition.
    CapSkipped(FanoutCapSkip),
    /// Mint exactly these cells.
    Mint(Vec<MintCell>),
}

fn slot_mint_plan(
    policy: &SnapshotPolicy,
    policy_name: &str,
    base_name: &str,
    members: Option<Vec<kopiur_api::expand::ExpandedMember>>,
) -> SlotMintPlan {
    if members.as_ref().is_some_and(Vec::is_empty) {
        return SlotMintPlan::NothingMatched;
    }
    let member_count = members.as_ref().map_or(1, Vec::len);
    let repo_count = policy.spec.repositories.len().max(1);
    if fanout_cap_exceeded(member_count, repo_count) {
        return SlotMintPlan::CapSkipped(FanoutCapSkip {
            policy: policy_name.to_string(),
            members: member_count,
            repos: repo_count,
        });
    }
    SlotMintPlan::Mint(mint_cells(policy, base_name, members))
}

/// Fire one `Snapshot` per (resolved target policy × source member ×
/// repository) for the slot. Each single-repo Snapshot's name is
/// `<schedule>-<policy>-<slot>` for the fan-out form (so a multi-policy
/// schedule doesn't collide), or `<schedule>-<slot>` for the single `policyRef`
/// form (preserving the existing idempotent name); multi-repo children ride
/// [`mint_cells`]' naming.
async fn fire_for_targets(
    ctx: &Context,
    schedule: &SnapshotSchedule,
    namespace: &str,
    slot: DateTime<Utc>,
) -> Result<FireOutcome> {
    // Whether ANY target policy fanned out (pvcSelector members and/or a
    // multi-repo dimension). The caller needs this because
    // `status.lastSchedule.snapshotRef` is reconstructed from the slot stamp
    // alone — under fan-out the real children are named `<base>-pvc-<slug>-<h8>`
    // (or `<base>-repo-…`), so that reconstruction would point at an object
    // that does not exist. A dangling ref is worse than none.
    let mut outcome = FireOutcome {
        fanned_out: false,
        cap_skipped: Vec::new(),
    };
    let targets = target_policy_refs(ctx, schedule, namespace).await?;
    let single = schedule.spec.policy_ref.is_some();
    let sched_name = schedule.name_any();
    for pref in &targets {
        let base_name = if single {
            scheduled_backup_name(&sched_name, slot)
        } else {
            format!("{sched_name}-{}-{}", pref.name, slot.format("%Y%m%d%H%M%S"))
        };
        // Expand `pvcSelector` sources into one child per matched PVC. A policy
        // with no selector yields `None` — mint exactly one unpinned child,
        // byte-for-byte the pre-#346 behavior.
        // Store-backed point read (#382 M2); a vanished policy maps to the
        // EXACT 404 error a bare `Api::get` used to raise here, so error
        // classification and the fire's skip/retry cadence are unchanged.
        let policy = io::fetch_policy(ctx, namespace, &pref.name)
            .await?
            .ok_or_else(|| fire_policy_not_found(&pref.name))?;
        let matched = crate::expand::match_pvcs(&ctx.client, &policy).await?;
        let members = crate::expand::expand_sources(&policy, &base_name, &matched)
            .map_err(|e| Error::Validation(e.to_string()))?;
        match slot_mint_plan(&policy, &pref.name, &base_name, members) {
            SlotMintPlan::NothingMatched => {
                outcome.fanned_out = true;
                // A selector that matched nothing is NOT an error — PVCs come
                // and go — but it is silent data loss if nobody says so, since
                // the schedule would look like it fired successfully.
                tracing::warn!(
                    schedule = %sched_name,
                    policy = %pref.name,
                    "pvcSelector matched no PersistentVolumeClaims; this slot backed up nothing"
                );
            }
            SlotMintPlan::CapSkipped(skip) => {
                tracing::warn!(
                    schedule = %sched_name,
                    policy = %pref.name,
                    members = skip.members,
                    repos = skip.repos,
                    cap = FANOUT_CAP,
                    "fan-out cross product exceeds the cap; skipping this slot for the policy"
                );
                outcome.fanned_out = true;
                outcome.cap_skipped.push(skip);
            }
            SlotMintPlan::Mint(cells) => {
                // Inherit the recipe's defaultDeletionPolicy so the produced
                // Snapshot carries it BEFORE admission (else the webhook stamps
                // its origin default) — #238. Resolved ONCE PER POLICY (audit
                // M10: it is a per-policy fact — the old per-child GET
                // multiplied apiserver reads by the fan-out width). A read
                // failure/missing/terminating policy propagates so the fire is
                // skipped and retried, never firing with a wrong (destructive)
                // default.
                let default_deletion_policy =
                    policy_default_deletion_policy(ctx, pref, namespace).await?;
                if cells.len() != 1 || cells[0].name != base_name {
                    outcome.fanned_out = true;
                }
                for cell in &cells {
                    create_scheduled_backup(
                        ctx,
                        schedule,
                        namespace,
                        cell,
                        pref,
                        default_deletion_policy,
                    )
                    .await?;
                }
            }
        }
    }
    Ok(outcome)
}

/// **Pure.** The error the fire loop raises when a targeted policy vanished
/// between `target_policy_refs` and the fire — byte-identical to the 404 a
/// bare `Api::<SnapshotPolicy>::get` used to raise there (#382 M2), so the
/// [`Error::Kube`] classification, requeue cadence, and event text are
/// preserved now that the read goes through the miss-is-live-confirmed
/// [`io::fetch_policy`] kernel (whose `None` IS a confirmed 404).
fn fire_policy_not_found(name: &str) -> Error {
    use kube::Resource;
    use kube::core::response::{StatusDetails, StatusSummary};
    let plural = SnapshotPolicy::plural(&());
    let group = SnapshotPolicy::group(&());
    Error::Kube(kube::Error::Api(Box::new(kube::core::Status {
        status: Some(StatusSummary::Failure),
        code: 404,
        message: format!("{plural}.{group} \"{name}\" not found"),
        reason: "NotFound".to_string(),
        details: Some(StatusDetails {
            name: name.to_string(),
            group: group.into_owned(),
            kind: plural.into_owned(),
            uid: String::new(),
            causes: Vec::new(),
            retry_after_seconds: 0,
        }),
        metadata: None,
    })))
}

/// `error_policy` for the `SnapshotSchedule` controller.
pub fn error_policy(obj: Arc<SnapshotSchedule>, err: &Error, ctx: Arc<Context>) -> Action {
    error_policy_for("SnapshotSchedule", obj.as_ref(), err, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use kopiur_api::SnapshotPhase;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).single().unwrap()
    }

    fn schedule_spec(
        cron: &str,
        suspend: bool,
        policy: ConcurrencyPolicy,
        deadline: Option<i64>,
    ) -> ScheduleSpec {
        ScheduleSpec {
            cron: cron.into(),
            jitter: None,
            timezone: None,
            run_on_create: false,
            suspend,
            concurrency_policy: policy,
            starting_deadline_seconds: deadline,
        }
    }

    #[test]
    fn slot_disposition_truth_table() {
        use ConcurrencyPolicy::{Allow, Forbid};
        let spec =
            |suspend, policy, deadline| schedule_spec("0 3 * * *", suspend, policy, deadline);
        let slot = at(2026, 8, 4, 3, 0);
        let before = at(2026, 8, 4, 2, 59);
        let soon_after = at(2026, 8, 4, 3, 5); // 300s past the slot
        let long_after = at(2026, 8, 4, 4, 0); // 3600s past the slot

        // Not yet due → Wait.
        assert_eq!(
            slot_disposition(&spec(false, Forbid, None), slot, before, false),
            SlotDisposition::Wait
        );
        // Due, no deadline → Fire (this is also the fire-once-on-recovery path:
        // a stale pin from an outage window fires exactly once).
        assert_eq!(
            slot_disposition(&spec(false, Forbid, None), slot, long_after, false),
            SlotDisposition::Fire
        );
        // Suspended → Wait, even when due (and even when expired: the pin is
        // frozen while suspended; unsuspending an expired slot skips it below).
        assert_eq!(
            slot_disposition(&spec(true, Forbid, Some(600)), slot, long_after, false),
            SlotDisposition::Wait
        );
        // Forbid + active run → Wait: the pinned slot is the single catch-up.
        assert_eq!(
            slot_disposition(&spec(false, Forbid, None), slot, soon_after, true),
            SlotDisposition::Wait
        );
        // Allow + active run → Fire (declared overlap contract).
        assert_eq!(
            slot_disposition(&spec(false, Allow, None), slot, soon_after, true),
            SlotDisposition::Fire
        );
        // Within the deadline → still fires.
        assert_eq!(
            slot_disposition(&spec(false, Forbid, Some(600)), slot, soon_after, false),
            SlotDisposition::Fire
        );
        // Expired past the deadline → SkipExpired (the #345 M1 regression: the
        // old boolean collapsed this into "don't fire", and the caller then
        // waited on a PAST slot at a 1s requeue floor forever).
        assert_eq!(
            slot_disposition(&spec(false, Forbid, Some(600)), slot, long_after, false),
            SlotDisposition::SkipExpired
        );
        // Deadline expiry wins over Forbid+active: a slot a long run pushed past
        // its deadline is skipped, not queued behind the run.
        assert_eq!(
            slot_disposition(&spec(false, Forbid, Some(600)), slot, long_after, true),
            SlotDisposition::SkipExpired
        );
        // The boolean view never fires an expired slot.
        assert!(!should_create_backup(
            &spec(false, Forbid, Some(600)),
            slot,
            long_after,
            false
        ));
    }

    #[test]
    fn scheduled_backup_spec_inherits_default_deletion_policy() {
        // Issue #238: the recipe's defaultDeletionPolicy must be stamped onto the
        // produced Snapshot, so a policy asking for Retain isn't silently
        // overridden to Delete by the webhook's origin-aware default.
        let pref = PolicyRef {
            name: "test-pvc".into(),
            namespace: None,
        };
        let retain = scheduled_backup_spec(
            &pref,
            Some(DeletionPolicy::Retain),
            ScheduleDeletePolicy::Retain,
            None,
            None,
        );
        assert_eq!(retain.deletion_policy, Some(DeletionPolicy::Retain));
        assert_eq!(
            retain.policy_ref.as_ref().map(|r| r.name.as_str()),
            Some("test-pvc")
        );

        let orphan = scheduled_backup_spec(
            &pref,
            Some(DeletionPolicy::Orphan),
            ScheduleDeletePolicy::Retain,
            None,
            None,
        );
        assert_eq!(orphan.deletion_policy, Some(DeletionPolicy::Orphan));

        // An unset recipe default leaves the field None, so the webhook's
        // safe origin-aware Delete default still applies (no behavior change).
        let unset = scheduled_backup_spec(&pref, None, ScheduleDeletePolicy::Retain, None, None);
        assert_eq!(unset.deletion_policy, None);
    }

    #[test]
    fn scheduled_backup_spec_stamps_on_schedule_delete() {
        let pref = PolicyRef {
            name: "pg".into(),
            namespace: None,
        };
        // The cascade policy is stamped EXPLICITLY (never left None) so the
        // finalizer's cascade guard reads a concrete value — both defaults.
        let retain = scheduled_backup_spec(&pref, None, ScheduleDeletePolicy::Retain, None, None);
        assert_eq!(
            retain.on_schedule_delete,
            Some(ScheduleDeletePolicy::Retain)
        );
        let delete = scheduled_backup_spec(&pref, None, ScheduleDeletePolicy::Delete, None, None);
        assert_eq!(
            delete.on_schedule_delete,
            Some(ScheduleDeletePolicy::Delete)
        );
        // The existing deletionPolicy threading is unchanged by the new param.
        let threaded = scheduled_backup_spec(
            &pref,
            Some(DeletionPolicy::Orphan),
            ScheduleDeletePolicy::Delete,
            None,
            None,
        );
        assert_eq!(threaded.deletion_policy, Some(DeletionPolicy::Orphan));
        assert_eq!(
            threaded.on_schedule_delete,
            Some(ScheduleDeletePolicy::Delete)
        );
    }

    /// A produced-child fixture with an optional stamped cascade value and an
    /// optional deletionTimestamp, shaped for `children_needing_cascade_stamp`.
    fn child(name: &str, stamped: Option<ScheduleDeletePolicy>, terminating: bool) -> Snapshot {
        let mut s = Snapshot::new(
            name,
            scheduled_backup_spec(
                &PolicyRef {
                    name: "pg".into(),
                    namespace: None,
                },
                None,
                stamped.unwrap_or(ScheduleDeletePolicy::Retain),
                None,
                None,
            ),
        );
        // scheduled_backup_spec always stamps Some(_); model an ABSENT stamp
        // (pre-upgrade child) by clearing it back to None when asked.
        s.spec.on_schedule_delete = stamped;
        s.metadata.namespace = Some("apps".into());
        if terminating {
            s.metadata.deletion_timestamp =
                Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                    k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
                ));
        }
        s
    }

    #[test]
    fn children_needing_cascade_stamp_selects_only_the_drifted_live_children() {
        let children = vec![
            child("differs", Some(ScheduleDeletePolicy::Retain), false), // Retain vs Delete → select
            child("matches", Some(ScheduleDeletePolicy::Delete), false), // already Delete → skip
            child("absent", None, false), // absent vs Delete → select
            child("terminating", Some(ScheduleDeletePolicy::Retain), true), // drifted but terminating → skip
        ];
        let selected = children_needing_cascade_stamp(&children, ScheduleDeletePolicy::Delete);
        assert_eq!(selected, vec!["differs".to_string(), "absent".to_string()]);
    }

    #[test]
    fn children_needing_cascade_stamp_skips_absent_when_desired_is_retain() {
        // Absent already resolves to Retain — stamping it would be pure churn.
        let children = vec![
            child("absent", None, false),
            child("already-retain", Some(ScheduleDeletePolicy::Retain), false),
            child("drifted-delete", Some(ScheduleDeletePolicy::Delete), false), // Delete vs Retain → select
        ];
        let selected = children_needing_cascade_stamp(&children, ScheduleDeletePolicy::Retain);
        assert_eq!(selected, vec!["drifted-delete".to_string()]);
    }

    #[test]
    fn slot_fire_blocked_only_by_a_terminating_slot_twin() {
        // No existing slot CR → fire.
        assert!(!slot_fire_blocked_by_terminating(None));
        // A live slot twin → fire (the server-side apply idempotently converges).
        let live = child("slot", Some(ScheduleDeletePolicy::Retain), false);
        assert!(!slot_fire_blocked_by_terminating(Some(&live)));
        // A terminating slot twin → SKIP (never fire-into-a-terminating-object).
        let terminating = child("slot", Some(ScheduleDeletePolicy::Retain), true);
        assert!(slot_fire_blocked_by_terminating(Some(&terminating)));
    }

    /// The policy read must FAIL the fire rather than degrade to a destructive
    /// default (#238 review): a `None` from a failed read reaches the webhook,
    /// which stamps `Delete` over the recipe's `Retain`/`Orphan` intent.
    mod policy_read_propagates_errors {
        use super::*;
        use http::{Request, Response, StatusCode};
        use kube::Client;
        use kube::client::Body;

        fn mock_client(status: StatusCode, body: serde_json::Value) -> Client {
            let body = Arc::new(body);
            let svc = tower::service_fn(move |_req: Request<Body>| {
                let body = body.clone();
                async move {
                    let resp = Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&*body).unwrap()))
                        .unwrap();
                    Ok::<_, std::convert::Infallible>(resp)
                }
            });
            Client::new(svc, "default")
        }

        fn status_body(code: u16, reason: &str) -> serde_json::Value {
            serde_json::json!({
                "kind": "Status", "apiVersion": "v1", "status": "Failure",
                "reason": reason, "code": code,
            })
        }

        fn pref() -> PolicyRef {
            PolicyRef {
                name: "test-pvc".into(),
                namespace: None,
            }
        }

        #[tokio::test]
        async fn transient_read_failure_errors_instead_of_defaulting() {
            // A 5xx must propagate so the fire is skipped/retried — never fire with
            // a wrong (Delete) default over a Retain/Orphan intent.
            let client = mock_client(
                StatusCode::INTERNAL_SERVER_ERROR,
                status_body(500, "InternalError"),
            );
            let ctx = Context::test_context(client);
            let r = policy_default_deletion_policy(&ctx, &pref(), "default").await;
            assert!(
                r.is_err(),
                "a failing policy read must propagate, got {r:?}"
            );
        }

        #[tokio::test]
        async fn missing_policy_is_missing_dependency() {
            // A genuinely-absent policy is an error too (mirrors the timezone
            // resolver) — the fire has nothing to inherit from, so skip/retry.
            let client = mock_client(StatusCode::NOT_FOUND, status_body(404, "NotFound"));
            let ctx = Context::test_context(client);
            let r = policy_default_deletion_policy(&ctx, &pref(), "default").await;
            assert!(
                matches!(r, Err(Error::MissingDependency(_))),
                "a missing policy must be MissingDependency, got {r:?}"
            );
        }

        #[tokio::test]
        async fn terminating_policy_is_missing_dependency() {
            // A policy mid-deletion must be treated EXACTLY like an absent one —
            // never fire a Snapshot into a recipe whose own deletion cascade may
            // already have LISTed (and so will never account for) this child.
            let body = serde_json::json!({
                "apiVersion": kopiur_api::consts::API_VERSION,
                "kind": "SnapshotPolicy",
                "metadata": {
                    "name": "test-pvc",
                    "namespace": "default",
                    "deletionTimestamp": "2024-01-01T00:00:00Z",
                },
                "spec": { "repository": { "name": "repo" } },
            });
            let client = mock_client(StatusCode::OK, body);
            let ctx = Context::test_context(client);
            let r = policy_default_deletion_policy(&ctx, &pref(), "default").await;
            assert!(
                matches!(r, Err(Error::MissingDependency(_))),
                "a terminating policy must be MissingDependency, got {r:?}"
            );
        }
    }

    /// A minimal `SnapshotPolicy` fixture with an optional deletionTimestamp,
    /// for [`policy_usable`].
    fn policy_fixture(terminating: bool) -> SnapshotPolicy {
        let mut p = SnapshotPolicy::new(
            "pg",
            kopiur_api::SnapshotPolicySpec {
                repository: Some(kopiur_api::common::RepositoryRef {
                    kind: Default::default(),
                    name: "repo".into(),
                    namespace: None,
                }),
                repositories: vec![],
                identity: None,
                sources: vec![],
                copy_method: Default::default(),
                volume_snapshot_class_name: None,
                staging: None,
                group_by: None,
                retention: None,
                default_deletion_policy: None,
                compression: None,
                files: None,
                extra_args: vec![],
                error_handling: None,
                upload: None,
                verification: None,
                preflight: None,
                suspend: false,
                hooks: None,
                mover: None,
                credential_projection: None,
                deletion: None,
                adoption: None,
            },
        );
        if terminating {
            p.metadata.deletion_timestamp =
                Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                    k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
                ));
        }
        p
    }

    #[test]
    fn policy_usable_true_when_live_false_when_terminating() {
        assert!(policy_usable(&policy_fixture(false)));
        assert!(!policy_usable(&policy_fixture(true)));
    }

    #[test]
    fn parse_go_duration_handles_units() {
        assert_eq!(parse_go_duration("30m"), Some(StdDuration::from_secs(1800)));
        assert_eq!(parse_go_duration("1h"), Some(StdDuration::from_secs(3600)));
        assert_eq!(parse_go_duration("45s"), Some(StdDuration::from_secs(45)));
        assert_eq!(parse_go_duration("120"), Some(StdDuration::from_secs(120)));
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("bogus"), None);
    }

    #[test]
    fn pin_recompute_only_when_recorded_zone_differs() {
        // Differing recorded zone → recompute (the pin is stale).
        assert!(pin_needs_recompute(Some("America/Chicago"), Tz::UTC));
        assert!(pin_needs_recompute(
            Some("UTC"),
            "Europe/Berlin".parse().unwrap()
        ));
    }

    #[test]
    fn pin_recompute_equal_zone_keeps_pin_no_churn() {
        // Determinism guard: identical zones never recompute (no jitter churn).
        assert!(!pin_needs_recompute(Some("UTC"), Tz::UTC));
        assert!(!pin_needs_recompute(
            Some("America/Chicago"),
            "America/Chicago".parse().unwrap()
        ));
    }

    #[test]
    fn pin_recompute_legacy_absent_zone_keeps_pin() {
        // A legacy pin (no recorded zone) is treated as unchanged — no upgrade churn.
        assert!(!pin_needs_recompute(None, Tz::UTC));
        assert!(!pin_needs_recompute(
            None,
            "Pacific/Kiritimati".parse().unwrap()
        ));
    }

    #[test]
    fn multi_repo_policy_disagreeing_repo_defaults_resolve_utc_with_ambiguity() {
        use kopiur_api::common::effective_timezone;
        // #368 audit M9: ONE policy, two repositories, disagreeing
        // `scheduleDefaults.timezone`. `policy_repo_timezone_default` now
        // contributes one entry per member, and those entries flow through the
        // SAME agree-or-UTC kernel as cross-policy defaults — so the outcome
        // is deterministic UTC plus the ambiguity that drives the existing
        // `TimezoneDefaultAmbiguous` warning condition.
        let per_repo = [
            Some("Europe/Berlin".to_string()),
            Some("America/Chicago".to_string()),
        ];
        let (tz, amb) = effective_timezone(None, &per_repo);
        assert_eq!(tz, chrono_tz::Tz::UTC);
        assert!(
            amb.is_some(),
            "within-policy disagreement must surface the warning ambiguity"
        );
        // All members agree → that value, no warning.
        let agree = [
            Some("Europe/Berlin".to_string()),
            Some("Europe/Berlin".to_string()),
        ];
        let (tz, amb) = effective_timezone(None, &agree);
        assert_eq!(tz.name(), "Europe/Berlin");
        assert!(amb.is_none());
    }

    fn resolved(name: &str) -> TimezoneResolution {
        TimezoneResolution::Resolved {
            tz: name.parse().unwrap(),
            ambiguity: None,
        }
    }

    #[test]
    fn degraded_keeps_established_non_utc_pin() {
        // REGRESSION (reviewer's flap concern): an established Europe/Berlin pin must
        // NOT be invalidated when timezone resolution degrades (a transient referent
        // failure). On the old `(Tz::UTC, None)`-on-failure code the caller could not
        // tell this from a resolved UTC, so `pin_needs_recompute(Some("Europe/Berlin"),
        // UTC)` fired and rewrote the pin to UTC timing — then flapped back on recovery.
        let (tz, ambiguity, needs_recompute) =
            resolve_pinned_slot_tz(Some("Europe/Berlin"), &TimezoneResolution::Degraded);
        assert!(!needs_recompute, "degrade must never invalidate a live pin");
        // The pin's own recorded zone stays in effect for this reconcile (no flap).
        assert_eq!(tz.name(), "Europe/Berlin");
        assert!(ambiguity.is_none());
    }

    #[test]
    fn resolved_differing_zone_invalidates_established_pin() {
        // A genuine resolution to a different zone still recomputes (the pin is stale).
        let (tz, _amb, needs_recompute) =
            resolve_pinned_slot_tz(Some("UTC"), &resolved("Europe/Berlin"));
        assert!(needs_recompute);
        assert_eq!(tz.name(), "Europe/Berlin");
        // Same zone resolved → no churn.
        let (_tz, _amb, again) =
            resolve_pinned_slot_tz(Some("Europe/Berlin"), &resolved("Europe/Berlin"));
        assert!(!again);
    }

    #[test]
    fn first_pin_degrade_then_recover_recomputes_exactly_once() {
        // (1) First reconcile while degraded: self-heal by pinning UTC now.
        let (tz0, amb0) = first_pin_tz(&TimezoneResolution::Degraded);
        assert_eq!(tz0.name(), "UTC");
        assert!(amb0.is_none());
        let pinned_tz = tz0.name().to_string(); // recorded on the pin = "UTC"

        // (2) Referents recover and resolve to the inherited Europe/Berlin: the
        // pinned-slot branch recomputes exactly once (UTC != Europe/Berlin).
        let (tz1, _amb1, recompute1) =
            resolve_pinned_slot_tz(Some(&pinned_tz), &resolved("Europe/Berlin"));
        assert!(
            recompute1,
            "recovery must recompute the UTC self-heal pin once"
        );
        assert_eq!(tz1.name(), "Europe/Berlin");
        let pinned_tz = tz1.name().to_string(); // re-pinned as Europe/Berlin

        // (3) Steady state: the same resolution no longer recomputes (stabilizes).
        let (_tz2, _amb2, recompute2) =
            resolve_pinned_slot_tz(Some(&pinned_tz), &resolved("Europe/Berlin"));
        assert!(
            !recompute2,
            "must stabilize — no repeated churn after recovery"
        );
    }

    #[test]
    fn first_pin_resolved_pins_that_zone() {
        let (tz, _amb) = first_pin_tz(&resolved("America/Chicago"));
        assert_eq!(tz.name(), "America/Chicago");
    }

    #[test]
    fn next_fire_is_deterministic_for_same_seed_and_after() {
        // 02:00 daily, no jitter. From 2026-05-24T03:00 the next slot is the
        // following day's 02:00.
        let after = at(2026, 5, 24, 3, 0);
        let a = next_fire("0 2 * * *", None, "uid-1", after, Tz::UTC).unwrap();
        let b = next_fire("0 2 * * *", None, "uid-1", after, Tz::UTC).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, at(2026, 5, 25, 2, 0));
    }

    #[test]
    fn next_fire_applies_deterministic_jitter_within_window() {
        let after = at(2026, 5, 24, 3, 0);
        let window = StdDuration::from_secs(1800); // 30m
        let fired = next_fire("0 2 * * *", Some(window), "uid-1", after, Tz::UTC).unwrap();
        let base = at(2026, 5, 25, 2, 0);
        let delta = (fired - base).num_seconds();
        assert!(
            (0..1800).contains(&delta),
            "jittered fire {fired} must be within [base, base+30m); delta={delta}"
        );
        // Deterministic: same inputs reproduce the exact same fire time.
        let again = next_fire("0 2 * * *", Some(window), "uid-1", after, Tz::UTC).unwrap();
        assert_eq!(fired, again);
    }

    #[test]
    fn next_fire_resolves_jenkins_h() {
        // `H 2 * * *` must parse (H resolved deterministically) and land at
        // some minute past 02:00.
        let after = at(2026, 5, 24, 3, 0);
        let fired = next_fire("H 2 * * *", None, "uid-x", after, Tz::UTC).unwrap();
        assert_eq!(fired.format("%H").to_string(), "02");
    }

    #[test]
    fn next_fire_rejects_bad_cron() {
        let after = at(2026, 5, 24, 3, 0);
        let err = next_fire("totally bad", None, "uid", after, Tz::UTC).unwrap_err();
        assert!(matches!(err, Error::InvalidSchedule(_)));
    }

    #[test]
    fn next_fire_evaluates_cron_in_the_given_timezone() {
        // `0 2 * * *` is "2am wall-clock". In America/Chicago during CDT (UTC-5,
        // summer) the next 2am after 2026-05-24T03:00Z (= 2026-05-23 22:00 CDT) is
        // 2026-05-24 02:00 CDT = 2026-05-24T07:00Z. UTC would have given 05-25 02:00Z.
        let after = at(2026, 5, 24, 3, 0);
        let chicago = next_fire("0 2 * * *", None, "uid-1", after, Tz::America__Chicago).unwrap();
        assert_eq!(chicago, at(2026, 5, 24, 7, 0));
        let utc = next_fire("0 2 * * *", None, "uid-1", after, Tz::UTC).unwrap();
        assert_eq!(utc, at(2026, 5, 25, 2, 0));
        assert_ne!(chicago, utc);
    }

    #[test]
    fn next_fire_is_dst_correct_across_the_spring_forward() {
        // US DST 2026 begins 2026-03-08 (clocks jump 02:00→03:00, CST→CDT). A 2am
        // daily cron after 2026-03-07T12:00Z must land on a real instant: the
        // 03-08 02:00 CST slot (= 08:00Z), not a skipped wall-clock time.
        let after = at(2026, 3, 7, 12, 0);
        let fired = next_fire("0 2 * * *", None, "uid-dst", after, Tz::America__Chicago).unwrap();
        assert_eq!(fired, at(2026, 3, 8, 8, 0));
    }

    #[test]
    fn run_on_create_fires_once_then_is_idempotent() {
        // runOnCreate set, not suspended, no prior run -> fire on create.
        let mut spec = schedule_spec("0 2 * * *", false, ConcurrencyPolicy::Allow, None);
        spec.run_on_create = true;
        assert!(should_run_on_create(&spec, false));
        // Once a run is recorded (status.lastSchedule present), never re-fire.
        assert!(!should_run_on_create(&spec, true));
    }

    #[test]
    fn run_on_create_defaults_off_and_respects_suspend() {
        // Default (runOnCreate unset) never fires on create.
        let off = schedule_spec("0 2 * * *", false, ConcurrencyPolicy::Allow, None);
        assert!(!should_run_on_create(&off, false));
        // Suspended schedules do not fire on create even with runOnCreate set.
        let mut suspended = schedule_spec("0 2 * * *", true, ConcurrencyPolicy::Allow, None);
        suspended.run_on_create = true;
        assert!(!should_run_on_create(&suspended, false));
    }

    #[test]
    fn suspend_blocks_creation() {
        let spec = schedule_spec("0 2 * * *", true, ConcurrencyPolicy::Allow, None);
        let slot = at(2026, 5, 24, 2, 0);
        let now = at(2026, 5, 24, 2, 1);
        assert!(!should_create_backup(&spec, slot, now, false));
    }

    #[test]
    fn forbid_skips_when_a_run_is_active() {
        let spec = schedule_spec("0 2 * * *", false, ConcurrencyPolicy::Forbid, None);
        let slot = at(2026, 5, 24, 2, 0);
        let now = at(2026, 5, 24, 2, 1);
        // Active run + Forbid → skip.
        assert!(!should_create_backup(&spec, slot, now, true));
        // No active run → proceed.
        assert!(should_create_backup(&spec, slot, now, false));
    }

    #[test]
    fn allow_and_replace_proceed_even_when_active() {
        for p in [ConcurrencyPolicy::Allow, ConcurrencyPolicy::Replace] {
            assert!(concurrency_allows(p, true));
        }
        assert!(!concurrency_allows(ConcurrencyPolicy::Forbid, true));
    }

    #[test]
    fn slot_not_due_yet_does_not_fire() {
        let spec = schedule_spec("0 2 * * *", false, ConcurrencyPolicy::Allow, None);
        let slot = at(2026, 5, 24, 2, 0);
        let now = at(2026, 5, 24, 1, 30); // before the slot
        assert!(!should_create_backup(&spec, slot, now, false));
    }

    #[test]
    fn policy_selector_match_decision() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::{
            LabelSelector, LabelSelectorRequirement,
        };
        let labels = BTreeMap::from([
            ("tier".to_string(), "critical".to_string()),
            ("app".to_string(), "pg".to_string()),
        ]);
        // matchLabels: exact value present → match.
        let ml = LabelSelector {
            match_labels: Some(BTreeMap::from([(
                "tier".to_string(),
                "critical".to_string(),
            )])),
            ..Default::default()
        };
        assert!(policy_matches_selector(&labels, &ml));
        // Wrong value → no match.
        let ml_wrong = LabelSelector {
            match_labels: Some(BTreeMap::from([("tier".to_string(), "low".to_string())])),
            ..Default::default()
        };
        assert!(!policy_matches_selector(&labels, &ml_wrong));
        // Empty selector matches everything.
        assert!(policy_matches_selector(&labels, &LabelSelector::default()));
        // matchExpressions: In / Exists / DoesNotExist.
        let me = LabelSelector {
            match_expressions: Some(vec![
                LabelSelectorRequirement {
                    key: "tier".into(),
                    operator: "In".into(),
                    values: Some(vec!["critical".into(), "high".into()]),
                },
                LabelSelectorRequirement {
                    key: "app".into(),
                    operator: "Exists".into(),
                    values: None,
                },
                LabelSelectorRequirement {
                    key: "deprecated".into(),
                    operator: "DoesNotExist".into(),
                    values: None,
                },
            ]),
            ..Default::default()
        };
        assert!(policy_matches_selector(&labels, &me));
        // NotIn that excludes the present value → no match.
        let not_in = LabelSelector {
            match_expressions: Some(vec![LabelSelectorRequirement {
                key: "tier".into(),
                operator: "NotIn".into(),
                values: Some(vec!["critical".into()]),
            }]),
            ..Default::default()
        };
        assert!(!policy_matches_selector(&labels, &not_in));
    }

    /// A minimal labeled policy in a namespace, for [`select_policy_targets`]
    /// (parsed the cluster's way: JSON value → typed).
    fn selectable_policy(
        ns: &str,
        name: &str,
        tier: Option<&str>,
        suspend: bool,
    ) -> SnapshotPolicy {
        let mut labels = serde_json::Map::new();
        if let Some(t) = tier {
            labels.insert("tier".to_string(), serde_json::Value::String(t.into()));
        }
        serde_json::from_value(serde_json::json!({
            "apiVersion": kopiur_api::consts::API_VERSION,
            "kind": "SnapshotPolicy",
            "metadata": { "name": name, "namespace": ns, "labels": labels },
            "spec": { "repository": { "name": "repo" }, "suspend": suspend },
        }))
        .expect("valid SnapshotPolicy fixture")
    }

    // --- select_policy_targets: the in-process replacement for the namespaced
    // selector LIST (#382 M2). The namespace filter INSIDE the kernel is the
    // audit-C4 guard: the reflector store is install-scope-wide, so a
    // label-only filter would merge another namespace's same-labeled policy
    // into this schedule's fan-out. ---
    #[test]
    fn select_policy_targets_filters_namespace_label_and_suspend() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
        let selector = LabelSelector {
            match_labels: Some(BTreeMap::from([(
                "tier".to_string(),
                "critical".to_string(),
            )])),
            ..Default::default()
        };
        let policies = [
            selectable_policy("team-a", "zz-match", Some("critical"), false),
            selectable_policy("team-a", "aa-match", Some("critical"), false),
            // C4: same labels, OTHER namespace — must be excluded.
            selectable_policy("team-b", "intruder", Some("critical"), false),
            // §14(e): suspended — must be excluded even when matching.
            selectable_policy("team-a", "paused", Some("critical"), true),
            // Label mismatch / unlabeled — excluded by the selector.
            selectable_policy("team-a", "wrong-tier", Some("low"), false),
            selectable_policy("team-a", "unlabeled", None, false),
        ];
        let refs = select_policy_targets(policies.iter(), "team-a", &selector);
        let names: Vec<_> = refs.iter().map(|r| r.name.as_str()).collect();
        // Sorted by name (parity with the apiserver's name-ordered LIST).
        assert_eq!(names, vec!["aa-match", "zz-match"]);
        assert!(
            refs.iter().all(|r| r.namespace.is_none()),
            "selector targets are namespace-local by design"
        );
        // An empty selector still honors namespace + suspend.
        let all = select_policy_targets(policies.iter(), "team-a", &LabelSelector::default());
        let all_names: Vec<_> = all.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            all_names,
            vec!["aa-match", "unlabeled", "wrong-tier", "zz-match"]
        );
    }

    // --- fire_policy_not_found: the store-miss → live-confirmed-404 mapping
    // must keep the EXACT error shape a bare `Api::get` raised (#382 M2), so
    // classification (Transient via Error::Kube) and messages are unchanged. ---
    #[test]
    fn fire_policy_not_found_matches_bare_get_404_shape() {
        let err = fire_policy_not_found("pg");
        match err {
            Error::Kube(kube::Error::Api(status)) => {
                assert_eq!(status.code, 404);
                assert_eq!(status.reason, "NotFound");
                assert_eq!(
                    status.message,
                    "snapshotpolicies.kopiur.home-operations.com \"pg\" not found"
                );
            }
            other => panic!("must be the kube Api 404 shape, got {other:?}"),
        }
    }

    #[test]
    fn missed_starting_deadline_skips() {
        let spec = schedule_spec("0 2 * * *", false, ConcurrencyPolicy::Allow, Some(600));
        let slot = at(2026, 5, 24, 2, 0);
        // 20 minutes late, deadline is 10 minutes → missed.
        let now = at(2026, 5, 24, 2, 20);
        assert!(missed_deadline(slot, now, Some(600)));
        assert!(!should_create_backup(&spec, slot, now, false));
        // Within deadline → fires.
        let now_ok = at(2026, 5, 24, 2, 5);
        assert!(should_create_backup(&spec, slot, now_ok, false));
    }

    #[test]
    fn failed_snapshots_to_prune_keeps_newest_and_skips_non_failed() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use kopiur_api::snapshot::{SnapshotPhase, SnapshotStatus, SnapshotTiming};

        fn snap(name: &str, phase: SnapshotPhase, end: &str) -> Snapshot {
            let mut s = Snapshot::new(
                name,
                SnapshotSpec {
                    repository: None,
                    source: None,
                    policy_ref: None,
                    tags: None,
                    failure_policy: None,
                    deletion_policy: None,
                    on_schedule_delete: None,
                    pin: false,
                    description: None,
                },
            );
            s.status = Some(SnapshotStatus {
                phase: Some(phase),
                timing: Some(SnapshotTiming {
                    end_time: Some(end.into()),
                    ..Default::default()
                }),
                ..Default::default()
            });
            s
        }

        let a = snap("a", SnapshotPhase::Failed, "2026-01-01T01:00:00Z");
        let b = snap("b", SnapshotPhase::Failed, "2026-01-01T02:00:00Z");
        let c = snap("c", SnapshotPhase::Failed, "2026-01-01T03:00:00Z");
        // A Succeeded snapshot must never be pruned by the failure limit.
        let ok = snap("ok", SnapshotPhase::Succeeded, "2026-01-01T09:00:00Z");
        // An already-terminating Failed snapshot is skipped (don't re-delete).
        let mut term = snap("term", SnapshotPhase::Failed, "2026-01-01T00:00:00Z");
        term.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
        // DATA-SAFETY: a Failed snapshot that produced a kopia snapshot (e.g. an
        // afterSnapshot hook aborted after creation) must NEVER be pruned — deleting
        // it would destroy a real backup. Even though it's the oldest, it stays.
        let mut with_artifact = snap("kept", SnapshotPhase::Failed, "2025-12-31T00:00:00Z");
        with_artifact.status.as_mut().unwrap().snapshot =
            Some(kopiur_api::snapshot::SnapshotInfo {
                kopia_snapshot_id: "kabc123".into(),
                identity: kopiur_api::common::ResolvedIdentity {
                    username: "u".into(),
                    hostname: "h".into(),
                    source_path: None,
                },
                description: None,
            });

        let all = vec![a, b, c, ok, term, with_artifact];
        // Keep the newest (c); prune the two older artifact-less failures; the
        // artifact-bearing "kept" is excluded entirely.
        let mut prune = failed_snapshots_to_prune(&all, 1);
        prune.sort();
        assert_eq!(prune, vec!["a".to_string(), "b".to_string()]);
        // 0 → prune every artifact-less, non-terminating failure (NOT "kept").
        let mut prune0 = failed_snapshots_to_prune(&all, 0);
        prune0.sort();
        assert_eq!(
            prune0,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(
            !prune0.contains(&"kept".to_string()),
            "a Failed snapshot owning a kopia snapshot must never be pruned"
        );
        // Limit ≥ count → no-op.
        assert!(failed_snapshots_to_prune(&all, 10).is_empty());
    }

    /// One SCHEDULE_LABEL population per reconcile (#382 M1): the same slice
    /// must coherently serve BOTH the concurrency gate and the failed-history
    /// prune — an unfinished run keeps `active=true` while the prune set from
    /// the very same slice still selects the artifact-less older failures.
    /// (Previously each consumer issued its own byte-identical LIST.)
    #[test]
    fn one_children_slice_serves_gate_and_prune_coherently() {
        use kopiur_api::snapshot::{SnapshotPhase, SnapshotStatus, SnapshotTiming};

        fn snap(name: &str, phase: SnapshotPhase, end: &str) -> Snapshot {
            let mut s = Snapshot::new(
                name,
                SnapshotSpec {
                    repository: None,
                    source: None,
                    policy_ref: None,
                    tags: None,
                    failure_policy: None,
                    deletion_policy: None,
                    on_schedule_delete: None,
                    pin: false,
                    description: None,
                },
            );
            s.status = Some(SnapshotStatus {
                phase: Some(phase),
                timing: Some(SnapshotTiming {
                    end_time: Some(end.into()),
                    ..Default::default()
                }),
                ..Default::default()
            });
            s
        }

        let slice = vec![
            snap("running", SnapshotPhase::Running, "2026-01-01T04:00:00Z"),
            snap("fail-old", SnapshotPhase::Failed, "2026-01-01T01:00:00Z"),
            snap("fail-new", SnapshotPhase::Failed, "2026-01-01T02:00:00Z"),
        ];

        let runs = classify_active_runs(&slice);
        assert!(
            runs.active,
            "the Running row must hold the Forbid gate shut"
        );
        assert!(runs.unreadable.is_none());

        let prune = failed_snapshots_to_prune(&slice, 1);
        assert_eq!(
            prune,
            vec!["fail-old".to_string()],
            "the same slice must still bound failure history (keep the newest Failed)"
        );
    }

    /// Multi-repo fan-out (#368): the failure-history bound is PER REPOSITORY
    /// (spec-pin repo_key; unpinned = one bucket). Two properties: each repo
    /// keeps `limit` of its own failures, and a flood of repo-B failures (an
    /// outage) can never evict repo-A's rarer, still-diagnostic records.
    #[test]
    fn failed_snapshots_to_prune_buckets_per_repository_pin() {
        use kopiur_api::common::{RepositoryKind, RepositoryRef};
        use kopiur_api::snapshot::{SnapshotPhase, SnapshotStatus, SnapshotTiming};

        fn snap(name: &str, repo: Option<&str>, end: &str) -> Snapshot {
            let mut s = Snapshot::new(
                name,
                SnapshotSpec {
                    repository: repo.map(|r| RepositoryRef {
                        kind: RepositoryKind::Repository,
                        name: r.into(),
                        namespace: Some("apps".into()),
                    }),
                    source: None,
                    policy_ref: None,
                    tags: None,
                    failure_policy: None,
                    deletion_policy: None,
                    on_schedule_delete: None,
                    pin: false,
                    description: None,
                },
            );
            s.metadata.namespace = Some("apps".into());
            s.status = Some(SnapshotStatus {
                phase: Some(SnapshotPhase::Failed),
                timing: Some(SnapshotTiming {
                    end_time: Some(end.into()),
                    ..Default::default()
                }),
                ..Default::default()
            });
            s
        }

        // Repo A: two old failures. Repo B: a newer four-failure flood.
        let all = vec![
            snap("a1", Some("repo-a"), "2026-01-01T01:00:00Z"),
            snap("a2", Some("repo-a"), "2026-01-01T02:00:00Z"),
            snap("b1", Some("repo-b"), "2026-01-02T01:00:00Z"),
            snap("b2", Some("repo-b"), "2026-01-02T02:00:00Z"),
            snap("b3", Some("repo-b"), "2026-01-02T03:00:00Z"),
            snap("b4", Some("repo-b"), "2026-01-02T04:00:00Z"),
        ];
        let mut prune = failed_snapshots_to_prune(&all, 1);
        prune.sort();
        // Each repo keeps ITS newest: a2 and b4 survive. Under the old flat
        // bound, limit 1 would have kept only b4 — repo B's outage flood
        // evicting every repo-A record.
        assert_eq!(prune, vec!["a1", "b1", "b2", "b3"]);

        // Unpinned rows are one flat bucket alongside the pinned ones.
        let mixed = vec![
            snap("u1", None, "2026-01-01T01:00:00Z"),
            snap("u2", None, "2026-01-01T02:00:00Z"),
            snap("b1", Some("repo-b"), "2026-01-02T01:00:00Z"),
        ];
        assert_eq!(failed_snapshots_to_prune(&mixed, 1), vec!["u1"]);
    }

    /// The fan-out cap guard (#368): the boundary arithmetic and the
    /// Stalled-style `FanoutCapped` condition wiring.
    #[test]
    fn fanout_cap_guard_and_condition() {
        // Boundary: exactly the cap is allowed; one past it is not.
        assert!(!fanout_cap_exceeded(400, 1));
        assert!(!fanout_cap_exceeded(50, 8));
        assert!(fanout_cap_exceeded(401, 1));
        assert!(fanout_cap_exceeded(51, 8));
        assert!(!fanout_cap_exceeded(0, 8));
        // Overflow-safe.
        assert!(fanout_cap_exceeded(usize::MAX, 2));

        let sched: SnapshotSchedule = serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotSchedule",
            "metadata": { "name": "nightly", "namespace": "apps", "generation": 3 },
            "spec": { "schedule": { "cron": "0 3 * * *" }, "policyRef": { "name": "pg" } }
        }))
        .expect("schedule fixture");
        let skips = vec![FanoutCapSkip {
            policy: "pg".into(),
            members: 60,
            repos: 8,
        }];
        let (conds, _) = schedule_ready_status(&sched, None, None, Some(&skips));
        let conds: Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> =
            serde_json::from_value(conds).expect("conditions decode");
        let capped = conds
            .iter()
            .find(|c| c.type_ == crate::consts::SCHEDULE_FANOUT_CAPPED_CONDITION)
            .expect("FanoutCapped present");
        assert_eq!(capped.status, "True");
        assert_eq!(capped.reason, crate::consts::FANOUT_TOO_LARGE_REASON);
        assert!(capped.message.contains("pg"), "{}", capped.message);
        assert!(capped.message.contains("480"), "{}", capped.message);

        // A clean fire (Some(&[])) self-clears it…
        let (conds, _) = schedule_ready_status(&sched, None, None, Some(&[]));
        let conds: Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> =
            serde_json::from_value(conds).expect("conditions decode");
        let capped = conds
            .iter()
            .find(|c| c.type_ == crate::consts::SCHEDULE_FANOUT_CAPPED_CONDITION)
            .expect("FanoutCapped present");
        assert_eq!(capped.status, "False");
        // …while a wait/hold pass (None) asserts nothing about it.
        let (conds, _) = schedule_ready_status(&sched, None, None, None);
        let conds: Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> =
            serde_json::from_value(conds).expect("conditions decode");
        assert!(
            !conds
                .iter()
                .any(|c| c.type_ == crate::consts::SCHEDULE_FANOUT_CAPPED_CONDITION)
        );
    }

    /// Golden: a single-repo scheduled child's spec is byte-identical to the
    /// pre-multi-repo wire (NO `repository` key), and a multi-repo cell's pin
    /// lands verbatim.
    #[test]
    fn scheduled_backup_spec_repository_pin_wire_golden() {
        use kopiur_api::common::{RepositoryKind, RepositoryRef};
        let pref = PolicyRef {
            name: "pg".into(),
            namespace: None,
        };
        let single = scheduled_backup_spec(&pref, None, ScheduleDeletePolicy::Retain, None, None);
        let wire = serde_json::to_value(&single).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "policyRef": { "name": "pg" },
                "onScheduleDelete": "Retain",
            }),
            "single-repo scheduled spec must stay byte-identical (no repository key)"
        );

        let pinned = scheduled_backup_spec(
            &pref,
            None,
            ScheduleDeletePolicy::Retain,
            None,
            Some(RepositoryRef {
                kind: RepositoryKind::Repository,
                name: "nas".into(),
                namespace: Some("apps".into()),
            }),
        );
        let wire = serde_json::to_value(&pinned).unwrap();
        assert_eq!(
            wire["repository"],
            serde_json::json!({ "kind": "Repository", "name": "nas", "namespace": "apps" })
        );
    }

    // --- concurrency gate: the version-skew livelock (#359 class) ------------

    fn run_in_phase(name: &str, phase: Option<SnapshotPhase>, deleting: bool) -> Snapshot {
        let mut s = Snapshot::new(
            name,
            SnapshotSpec {
                repository: None,
                source: None,
                policy_ref: None,
                tags: None,
                failure_policy: None,
                deletion_policy: None,
                on_schedule_delete: None,
                pin: false,
                description: None,
            },
        );
        s.status = Some(kopiur_api::snapshot::SnapshotStatus {
            phase,
            ..Default::default()
        });
        if deleting {
            s.metadata.deletion_timestamp =
                Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                    k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
                ));
        }
        s
    }

    #[test]
    fn classify_active_runs_matches_the_old_unfinished_set() {
        // Terminal children never hold the gate.
        for terminal in [
            SnapshotPhase::Succeeded,
            SnapshotPhase::Failed,
            SnapshotPhase::Deleting,
            SnapshotPhase::Discovered,
            SnapshotPhase::Unchanged,
        ] {
            let runs = classify_active_runs(&[run_in_phase("a", Some(terminal.clone()), false)]);
            assert_eq!(runs, ActiveRuns::default(), "{terminal:?}");
        }
        // In-flight (and status-less) children do.
        for unfinished in [
            None,
            Some(SnapshotPhase::Pending),
            Some(SnapshotPhase::Running),
        ] {
            let runs = classify_active_runs(&[run_in_phase("a", unfinished.clone(), false)]);
            assert!(runs.active, "{unfinished:?}");
            assert_eq!(runs.unreadable, None, "only an Unknown phase is unreadable");
        }
        // A terminating child is excluded outright, whatever its phase.
        assert_eq!(
            classify_active_runs(&[run_in_phase("a", Some(SnapshotPhase::Running), true)]),
            ActiveRuns::default()
        );
    }

    #[test]
    fn an_unreadable_phase_holds_the_gate_and_is_named() {
        // Fails CLOSED (it may be a live run under a newer operator) — but this
        // build can never see it finish, so the blocker must be NAMED, not just
        // counted, or the schedule stops firing with nothing saying why (#359).
        let runs = classify_active_runs(&[run_in_phase(
            "nightly-20260807",
            Some(SnapshotPhase::Unknown("Quiescing".into())),
            true, // terminating: excluded, so this one must NOT register
        )]);
        assert_eq!(
            runs,
            ActiveRuns::default(),
            "a terminating child is skipped"
        );

        let runs = classify_active_runs(&[
            run_in_phase("finished", Some(SnapshotPhase::Succeeded), false),
            run_in_phase(
                "nightly-20260807",
                Some(SnapshotPhase::Unknown("Quiescing".into())),
                false,
            ),
        ]);
        assert!(runs.active);
        assert_eq!(
            runs.unreadable,
            Some(UnreadableRun {
                snapshot: "nightly-20260807".into(),
                phase: "Quiescing".into(),
            })
        );

        // Under the default `Forbid` this is what stops the schedule dead: the
        // gate says Wait, and no future reconcile of THIS build can change that.
        let spec = schedule_spec("0 3 * * *", false, ConcurrencyPolicy::Forbid, None);
        assert_eq!(
            slot_disposition(
                &spec,
                at(2026, 8, 4, 3, 0),
                at(2026, 8, 4, 3, 5),
                runs.active
            ),
            SlotDisposition::Wait
        );
        // `Allow` declares overlap, so an unreadable run does not block it — the
        // reconciler's `blocker` filter keys on the disposition for exactly this.
        let allow = schedule_spec("0 3 * * *", false, ConcurrencyPolicy::Allow, None);
        assert_eq!(
            slot_disposition(
                &allow,
                at(2026, 8, 4, 3, 0),
                at(2026, 8, 4, 3, 5),
                runs.active
            ),
            SlotDisposition::Fire
        );
    }

    #[test]
    fn the_runnable_gate_is_set_and_cleared_from_the_same_fact() {
        use crate::consts::{BLOCKED_ON_UNREADABLE_RUN_REASON, SCHEDULE_RUNNABLE_CONDITION};
        let mut sched = SnapshotSchedule::new(
            "nightly",
            kopiur_api::SnapshotScheduleSpec {
                policy_ref: None,
                policy_selector: None,
                schedule: schedule_spec("0 3 * * *", false, ConcurrencyPolicy::Forbid, None),
                failed_jobs_history_limit: None,
                deletion: None,
            },
        );
        sched.metadata.namespace = Some("apps".into());
        sched.metadata.generation = Some(2);

        let blocked = UnreadableRun {
            snapshot: "nightly-20260807".into(),
            phase: "Quiescing".into(),
        };
        let (conditions, _) = schedule_ready_status(&sched, None, Some(&blocked), None);
        let conds: Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> =
            serde_json::from_value(conditions).unwrap();
        let gate = conds
            .iter()
            .find(|c| c.type_ == SCHEDULE_RUNNABLE_CONDITION)
            .expect("the runnable gate is written");
        assert_eq!(gate.status, "False");
        assert_eq!(gate.reason, BLOCKED_ON_UNREADABLE_RUN_REASON);
        // The message must name BOTH the blocking Snapshot and the raw phase —
        // without them the operator cannot act on it.
        assert!(
            gate.message.contains("nightly-20260807"),
            "{}",
            gate.message
        );
        assert!(gate.message.contains("Quiescing"), "{}", gate.message);

        // The registry row and the writer agree — the whole point of #359's
        // shared-by-construction fix. (M3's doctor reads the same row.)
        let row = kopiur_api::gates::STRUCTURAL_GATES
            .iter()
            .find(|g| g.matches(&gate.type_, &gate.status, &gate.reason))
            .expect("the gate this reconciler writes must be registered");
        assert!(row.applies_to.covers_snapshot_schedule());

        // Clearing: the SAME function with no blocker flips it back, so a
        // resolved outage does not read as permanent.
        sched.status = Some(kopiur_api::SnapshotScheduleStatus {
            conditions: conds,
            ..Default::default()
        });
        assert!(recorded_blocked_on_unreadable(&sched));
        let (cleared, _) = schedule_ready_status(&sched, None, None, None);
        let cleared: Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> =
            serde_json::from_value(cleared).unwrap();
        let gate = cleared
            .iter()
            .find(|c| c.type_ == SCHEDULE_RUNNABLE_CONDITION)
            .expect("still present, just True");
        assert_eq!(gate.status, "True");
        sched.status = Some(kopiur_api::SnapshotScheduleStatus {
            conditions: cleared,
            ..Default::default()
        });
        assert!(!recorded_blocked_on_unreadable(&sched));
    }

    // --- #382 M3: store-served schedule children ----------------------------

    use http::{Request, Response, StatusCode};
    use kube::client::Body;

    /// Logs `"<METHOD> <path>"` per request; answers with `status` + `body`.
    fn logging_client(
        log: Arc<std::sync::Mutex<Vec<String>>>,
        status: StatusCode,
        body: serde_json::Value,
    ) -> kube::Client {
        let body = Arc::new(body);
        let svc = tower::service_fn(move |req: Request<Body>| {
            let log = log.clone();
            let body = body.clone();
            async move {
                log.lock()
                    .unwrap()
                    .push(format!("{} {}", req.method(), req.uri().path()));
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&*body).unwrap()))
                        .unwrap(),
                )
            }
        });
        kube::Client::new(svc, "default")
    }

    fn labeled_child(ns: &str, name: &str, schedule: &str) -> Snapshot {
        let mut s: Snapshot = serde_json::from_value(serde_json::json!({
            "apiVersion": crate::consts::API_VERSION,
            "kind": "Snapshot",
            "metadata": {
                "name": name, "namespace": ns, "uid": format!("uid-{ns}-{name}"),
                "labels": { (crate::consts::SCHEDULE_LABEL): schedule },
            },
            "spec": {},
        }))
        .expect("valid Snapshot fixture");
        s.status = Some(kopiur_api::snapshot::SnapshotStatus {
            phase: Some(SnapshotPhase::Failed),
            ..Default::default()
        });
        s
    }

    fn prime_snapshot_store(ctx: &Context, objs: Vec<Snapshot>, synced: bool) {
        use std::sync::atomic::Ordering;
        let (store, mut writer) = kube::runtime::reflector::store::<Snapshot>();
        for o in objs {
            writer.apply_watcher_event(&kube::runtime::watcher::Event::Apply(o));
        }
        std::mem::forget(writer);
        ctx.snapshot_store.set(store).ok();
        ctx.snapshot_synced.store(synced, Ordering::Release);
    }

    /// C4: the schedule-children population served from the install-scope-wide
    /// store must filter namespace AND label — another namespace's same-named
    /// schedule must not feed this schedule's Forbid gate, prune set, or
    /// cascade propagation.
    #[tokio::test]
    async fn schedule_children_from_store_exclude_cross_namespace_intruders() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = logging_client(
            log.clone(),
            StatusCode::NOT_FOUND,
            serde_json::json!({ "kind": "Status", "code": 404 }),
        );
        let ctx = Context::test_context(client);
        prime_snapshot_store(
            &ctx,
            vec![
                labeled_child("team-a", "run-1", "nightly"),
                labeled_child("team-a", "run-2", "nightly"),
                // Same schedule NAME, another namespace: must not leak in.
                labeled_child("team-b", "intruder", "nightly"),
                labeled_child("team-a", "other-sched", "weekly"),
            ],
            true,
        );

        let mut got: Vec<String> = schedule_children(&ctx, "team-a", "nightly")
            .await
            .unwrap()
            .iter()
            .map(|s| s.name_any())
            .collect();
        got.sort();
        assert_eq!(got, vec!["run-1", "run-2"]);
        assert!(
            log.lock().unwrap().is_empty(),
            "a synced store serves the children with zero HTTP"
        );
    }

    /// C2: the failed-history prune executor live-verifies each store-selected
    /// row before destruction — a vanished row is skipped: one GET, no
    /// stamp-PATCH, no DELETE.
    #[tokio::test]
    async fn prune_failed_history_live_verifies_before_deleting() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = logging_client(
            log.clone(),
            StatusCode::NOT_FOUND,
            serde_json::json!({
                "kind": "Status", "apiVersion": "v1", "status": "Failure",
                "reason": "NotFound", "code": 404,
            }),
        );
        let ctx = Context::test_context(client);
        let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), "team-a");
        // Two artifact-less Failed rows over limit 0 → both selected.
        let children = vec![
            labeled_child("team-a", "fail-1", "nightly"),
            labeled_child("team-a", "fail-2", "nightly"),
        ];

        prune_failed_history(&api, "nightly", &children, Some(0))
            .await
            .unwrap();
        let requests = log.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            2,
            "exactly one verify GET per selected row, got {requests:?}"
        );
        assert!(
            requests.iter().all(|r| r.starts_with("GET ")),
            "vanished rows must produce NO stamp/delete traffic: {requests:?}"
        );
    }
}
