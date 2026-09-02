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
    DeletionPolicy, PolicyRef, RepositoryRef, ScheduleDefaults, ScheduleDeletePolicy,
    ScheduleJitterResolution, TimezoneAmbiguity, effective_timezone, repo_key,
    resolve_schedule_jitter, resolve_tz,
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

/// What a pinned `nextSchedule` recorded about *how* its wall-clock slot was
/// computed — the two inputs to [`next_fire`] that can change underneath a pin
/// because either may be INHERITED from the target repository's `scheduleDefaults`.
/// Both are `None` on legacy pins written before the respective field existed.
///
/// A struct rather than two adjacent `Option<&str>` parameters: they are the same
/// type and would silently swap at a call site.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PinnedSlot<'a> {
    /// `status.nextSchedule.timezone` — the IANA zone the cron was evaluated in.
    pub timezone: Option<&'a str>,
    /// `status.nextSchedule.jitter` — the Go-style jitter window the slot was
    /// spread by (`None` also means "this slot carried no jitter").
    pub jitter: Option<&'a str>,
}

/// **Pure.** Whether a pinned `nextSchedule` slot must be recomputed because the
/// effective cron *timing inputs* changed since it was pinned: the timezone
/// (`pinned.timezone` vs `effective_tz`) or the deterministic jitter window
/// (`pinned.jitter` vs `effective_jitter`). Either may change without any edit to
/// this `SnapshotSchedule` — both are inherited from the target repository's
/// `scheduleDefaults` when the schedule doesn't set its own — so without this the
/// edit would only take effect an arbitrary slot later.
///
/// Determinism guard, applied to BOTH inputs independently: returns `false` for an
/// equal value AND for an absent recorded value, so the steady state never
/// recomputes — no jitter churn on every reconcile, and no one-time churn for
/// schedules upgraded across the addition of either field. A recompute is triggered
/// only by an observed, recorded value that actually differs.
///
/// The jitter comparison is on the PARSED window, not the raw string, so a
/// re-spelling (`60m` → `1h`) is correctly seen as unchanged: a needless recompute
/// is not free — it re-anchors `next_fire` at `now`, which can skip a slot whose
/// jitter offset has not yet elapsed.
pub fn pin_needs_recompute(
    pinned: PinnedSlot<'_>,
    effective_tz: Tz,
    effective_jitter: Option<&str>,
) -> bool {
    let tz_changed = pinned.timezone.is_some_and(|p| p != effective_tz.name());
    let jitter_changed = pinned
        .jitter
        .is_some_and(|p| jitter_window(Some(p)) != jitter_window(effective_jitter));
    tz_changed || jitter_changed
}

/// Test helper: a repository `scheduleDefaults` carrying ONLY a timezone — the
/// pre-jitter shape, and therefore the byte-identical-regression baseline every
/// consumer's inheritance test is written against. Lives beside the scheduling
/// kernel (`next_fire`, `parse_go_duration`) that all five cron consumers import,
/// so there is one definition rather than five.
#[cfg(test)]
pub(crate) fn tz_defaults(timezone: &str) -> ScheduleDefaults {
    ScheduleDefaults {
        timezone: Some(timezone.to_string()),
        jitter: None,
    }
}

/// Test helper: a repository `scheduleDefaults` carrying ONLY a jitter window.
#[cfg(test)]
pub(crate) fn jitter_defaults(jitter: &str) -> ScheduleDefaults {
    ScheduleDefaults {
        timezone: None,
        jitter: Some(jitter.to_string()),
    }
}

/// Parse a raw jitter string into the window [`next_fire`] consumes. One place, so
/// the pin comparison and the actual slot computation can never disagree about what
/// a given string means (an unparseable value is no jitter — the webhook rejects
/// those at admission, at both the schedule and the `scheduleDefaults` level).
fn jitter_window(raw: Option<&str>) -> Option<StdDuration> {
    raw.and_then(parse_go_duration)
}

/// Outcome of resolving a `SnapshotSchedule`'s effective cron timing inputs
/// (timezone AND jitter window) for one reconcile. Distinguishes a genuine
/// resolution (referents read successfully) from a **degraded** pass (a referent
/// GET/list failed, or a matched policy/repo was missing) so the caller can honor
/// the invariant that *a transient referent failure must never invalidate an
/// established pin*: without this distinction the old `(Tz::UTC, None)`-on-failure
/// return was indistinguishable from a genuinely-resolved UTC, so an apiserver blip
/// would flap a `Europe/Berlin` pin to UTC timing and back. Jitter rides in the same
/// value rather than a parallel one precisely so it inherits that guarantee.
/// Internal to the reconciler — not serialized, so the status schema is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScheduleResolution {
    /// Referents were read. `tz` is the effective zone and `ambiguity` is `Some` when
    /// matched policies' repositories disagreed on their timezone default (UTC in
    /// effect + a warn-only status condition); `jitter` is the effective window
    /// (`None` = no jitter, which is also where a jitter *disagreement* lands — it is
    /// warn-logged at resolution time rather than surfaced as a condition).
    Resolved {
        tz: Tz,
        ambiguity: Option<TimezoneAmbiguity>,
        jitter: Option<String>,
    },
    /// Resolution could not complete this reconcile (referent GET/list failure or a
    /// missing policy/repo). The controller keeps an established pin untouched and
    /// only self-heals a *first* pin.
    ///
    /// `own_tz` / `own_jitter` carry the schedule's OWN `spec.schedule.timezone` /
    /// `spec.schedule.jitter` when it set them. **An own value is not a lookup
    /// result**, so those halves are NOT degraded — only the genuinely inherited
    /// ones are unknown. Both are carried for the same reason and must stay
    /// symmetric: a schedule whose policy/repository is momentarily unreadable (a
    /// routine GitOps bundle-apply ordering, and a lookup this reconciler now makes
    /// for the jitter half even when the timezone is explicit) would otherwise
    /// first-pin UTC / UN-jittered instead of what the user wrote down, and an
    /// explicit EDIT to either would not invalidate its pin.
    ///
    /// The jitter half is the sharper of the two: because [`pin_needs_recompute`]'s
    /// jitter arm is `is_some_and` on the PINNED side (the upgrade-churn rule), a
    /// first pin that recorded no window is never revisited on recovery — so an
    /// un-jittered self-heal pin for a schedule that explicitly asked for jitter
    /// would be PERMANENT for that pin, which is exactly the stampede jitter exists
    /// to prevent.
    Degraded {
        own_tz: Option<Tz>,
        own_jitter: Option<String>,
    },
}

/// The cron timing inputs a slot is computed and pinned with for one reconcile.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EffectiveSchedule {
    tz: Tz,
    ambiguity: Option<TimezoneAmbiguity>,
    jitter: Option<String>,
}

impl EffectiveSchedule {
    /// The parsed window to hand [`next_fire`].
    fn window(&self) -> Option<StdDuration> {
        jitter_window(self.jitter.as_deref())
    }

    /// The `status.nextSchedule` object to pin. `jitter` is emitted as JSON `null`
    /// when no jitter applies, which — under the merge patch `io::patch_status`
    /// issues — DELETES a stale recorded window rather than leaving it to
    /// (incorrectly) invalidate every future pin.
    fn pin_json(&self, at: DateTime<Utc>) -> serde_json::Value {
        serde_json::json!({
            "at": at.to_rfc3339(),
            "timezone": self.tz.name(),
            "jitter": self.jitter,
        })
    }
}

/// **Pure.** Decide the effective timing inputs, ambiguity signal, and whether the
/// pinned `nextSchedule` slot must be recomputed, given the pin's recorded values
/// (`pinned`) and this reconcile's [`ScheduleResolution`]. Exhaustive over the
/// resolution — no `_ =>`:
///
/// - `Resolved { .. }`: the pin is invalidated iff a recorded value actually differs
///   (via [`pin_needs_recompute`]); the resolved zone/window/ambiguity flow on to the
///   re-pin and status.
/// - `Degraded { own_tz, own_jitter }`: a transient referent failure must **never**
///   invalidate an established pin *on the halves it could not read*, so each
///   INHERITED half is held at the pin's own recorded value and is structurally
///   incapable of triggering a recompute (it is masked to absent on both sides of
///   the comparison). A half the schedule set ITSELF needed no lookup, so it stays
///   authoritative and an edit to it still invalidates the pin — unchanged from
///   before jitter inheritance made this function reachable while degraded. A legacy
///   pin with no recorded zone and no own zone resolves to UTC via [`resolve_tz`].
///   No ambiguity is asserted while degraded.
fn resolve_pinned_slot(
    pinned: PinnedSlot<'_>,
    resolution: &ScheduleResolution,
) -> (EffectiveSchedule, bool) {
    match resolution {
        ScheduleResolution::Resolved {
            tz,
            ambiguity,
            jitter,
        } => (
            EffectiveSchedule {
                tz: *tz,
                ambiguity: ambiguity.clone(),
                jitter: jitter.clone(),
            },
            pin_needs_recompute(pinned, *tz, jitter.as_deref()),
        ),
        ScheduleResolution::Degraded { own_tz, own_jitter } => {
            // Per half: the schedule's own value if it set one (authoritative — no
            // lookup was needed), else the pin's own recorded value (held, because
            // this pass could not read what it would have inherited).
            let tz = own_tz.unwrap_or_else(|| resolve_tz(pinned.timezone));
            let jitter = own_jitter
                .clone()
                .or_else(|| pinned.jitter.map(str::to_string));
            // Mask each INHERITED half to absent on the pinned side: an absent
            // recorded value never recomputes, so an unreadable referent cannot move
            // the pin. An OWN half is compared for real.
            let masked = PinnedSlot {
                timezone: own_tz.and(pinned.timezone),
                jitter: own_jitter.as_ref().and(pinned.jitter),
            };
            let needs_recompute = pin_needs_recompute(masked, tz, jitter.as_deref());
            (
                EffectiveSchedule {
                    tz,
                    ambiguity: None,
                    jitter,
                },
                needs_recompute,
            )
        }
    }
}

/// **Pure.** The timing inputs to pin on the FIRST reconcile (no pin recorded yet).
/// Exhaustive over [`ScheduleResolution`] — no `_ =>`:
/// - `Resolved { .. }`: pin that zone and window (and surface any ambiguity).
/// - `Degraded { own_tz, own_jitter }`: pin each half the schedule set ITSELF (no
///   lookup was needed for those), and self-heal the inherited ones — UTC, no
///   jitter. Once referents recover, the pinned-slot branch recomputes into the
///   inherited zone/window (then stabilizes — see [`resolve_pinned_slot`]).
///
///   Pinning `own_jitter` here rather than `None` is load-bearing, not tidiness:
///   [`pin_needs_recompute`]'s jitter arm only fires on a PINNED window that
///   differs, so a first pin that recorded none is never revisited — an
///   un-jittered self-heal would be permanent for a schedule that explicitly asked
///   for a window.
fn first_pin(resolution: &ScheduleResolution) -> EffectiveSchedule {
    match resolution {
        ScheduleResolution::Resolved {
            tz,
            ambiguity,
            jitter,
        } => EffectiveSchedule {
            tz: *tz,
            ambiguity: ambiguity.clone(),
            jitter: jitter.clone(),
        },
        ScheduleResolution::Degraded { own_tz, own_jitter } => EffectiveSchedule {
            tz: own_tz.unwrap_or(Tz::UTC),
            ambiguity: None,
            jitter: own_jitter.clone(),
        },
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
/// is currently active. `Forbid` skips when active; `Allow`/`Replace` proceed.
///
/// `Replace` returning `true` here is deliberate and is only half the answer:
/// it means "a due slot is not simply skipped", not "fire unconditionally".
/// What the replacement actually does with the in-flight run — and the two
/// cases where it must NOT fire after all (an unreadable-phase child, and a
/// child parked behind the repository concurrency cap) — is decided by
/// [`replace_plan`], which the reconciler consults before it commits the Fire.
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
    hold: Option<ScheduleHold<'_>>,
    fanout_cap: Option<&[FanoutCapSkip]>,
) -> (serde_json::Value, i64) {
    use crate::consts::{
        BLOCKED_ON_UNREADABLE_RUN_REASON, FANOUT_TOO_LARGE_REASON, FANOUT_WITHIN_CAP_REASON,
        SCHEDULE_FANOUT_CAPPED_CONDITION, SCHEDULE_RUNNABLE_CONDITION,
        SCHEDULE_TIMEZONE_AMBIGUOUS_CONDITION, SCHEDULE_TIMEZONE_AMBIGUOUS_REASON,
        SCHEDULE_TIMEZONE_RESOLVED_REASON,
    };
    use kopiur_api::consts::{
        REPLACEMENT_NOT_HELD_REASON, SCHEDULE_REPLACEMENT_HELD_CONDITION,
        WAITING_FOR_REPOSITORY_SLOT_REASON,
    };
    // Split the one hold into the two independent conditions it feeds, so each
    // is written from a single place with BOTH polarities (a set-only condition
    // would report a resolved outage forever). Exhaustive over `ScheduleHold`.
    let (blocked, parked) = match hold {
        None => (None, None),
        Some(ScheduleHold::Unreadable(b)) => (Some(b), None),
        Some(ScheduleHold::ParkedRun(name)) => (None, Some(name)),
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
    // The `Replace`-held-by-a-parked-run marker. Both polarities, same reason as
    // the runnable gate: it is the transition guard for the Normal event, so a
    // stale `True` would suppress the event forever once the pool drained and
    // re-saturated. NOT a structural gate — it needs no human and self-clears.
    let (parked_reason, parked_message) = match parked {
        Some(run) => (
            WAITING_FOR_REPOSITORY_SLOT_REASON,
            format!(
                "concurrencyPolicy: Replace is holding this slot: the run it would replace \
                 (`{run}`) is itself queued behind its repository's mover-Job concurrency cap, \
                 so cancelling it would free no capacity and the replacement would re-queue \
                 behind it. This clears itself when the pool drains — no action needed."
            ),
        ),
        None => (
            REPLACEMENT_NOT_HELD_REASON,
            "no replacement is held behind the repository's concurrency cap".to_string(),
        ),
    };
    let conditions = io::upsert_condition(
        &conditions,
        SCHEDULE_REPLACEMENT_HELD_CONDITION,
        parked.is_some(),
        parked_reason,
        &parked_message,
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

/// Why a DUE slot is being held shut this reconcile — the single input to the
/// two hold conditions [`schedule_ready_status`] writes. An enum rather than two
/// `Option`s because the states are mutually exclusive by construction
/// ([`replace_plan`] returns one plan) and an exhaustive match is what forces a
/// future hold to declare which condition surfaces it.
#[derive(Debug, Clone, Copy)]
enum ScheduleHold<'a> {
    /// A run at a phase this build cannot read — `ScheduleRunnable=False`, the
    /// registered structural gate. Needs a human; never self-clears.
    Unreadable(&'a UnreadableRun),
    /// `concurrencyPolicy: Replace` waiting on a run parked behind its
    /// repository's concurrency cap — `ReplacementHeld=True`. Self-clears.
    ParkedRun(&'a str),
}

/// Whether `status.conditions` already records the `Replace`-held-by-parked-run
/// state. The transition guard for its Normal Event, mirroring
/// [`recorded_blocked_on_unreadable`]: the held branch re-runs on every requeue
/// (at the 1s floor, since the slot is already due), so an Event per pass would
/// bury the namespace's event log within seconds.
fn recorded_held_by_parked_run(schedule: &SnapshotSchedule) -> bool {
    schedule
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or_default()
        .iter()
        .find(|c| c.type_ == kopiur_api::consts::SCHEDULE_REPLACEMENT_HELD_CONDITION)
        .is_some_and(|c| c.status == "True")
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
    // Effective cron TIMING resolution for this reconcile — timezone AND jitter
    // window. When the schedule sets its own `spec.schedule.{timezone,jitter}`, each
    // wins with no lookups for that half. Otherwise the half is inherited from the
    // target policies' repository `scheduleDefaults` (timezone: agree-or-UTC, a
    // disagreement among selector matches degrades to UTC + a status condition;
    // jitter: agree-or-none, a disagreement warn-logs and applies no jitter). A
    // referent GET/list failure (or a missing policy/repo) yields `Degraded` — which
    // must NOT invalidate an established pin (a transient apiserver blip would
    // otherwise flap the pinned slot to UTC/no-jitter timing and back); it only
    // self-heals a first pin. See `resolve_pinned_slot` / `first_pin`.
    let resolution = resolve_effective_schedule(ctx, schedule, &namespace).await;

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
    // The timezone and jitter window this slot was pinned with. Absent on legacy pins
    // (written before the respective field existed) → treated as "unchanged" so an
    // upgrade never churns the pin.
    let pinned_tz = pinned.and_then(|r| r.timezone.clone());
    let pinned_jitter = pinned.and_then(|r| r.jitter.clone());

    if let Some(slot) = pinned_slot {
        // Effective zone/window + whether the pin is stale, honoring Degraded
        // semantics: `Resolved` invalidates iff a recorded value actually changed;
        // `Degraded` keeps the pin's own zone AND window and never invalidates (no
        // flap on a referent blip).
        let recorded = PinnedSlot {
            timezone: pinned_tz.as_deref(),
            jitter: pinned_jitter.as_deref(),
        };
        let (eff, needs_recompute) = resolve_pinned_slot(recorded, &resolution);
        let (tz, tz_ambiguity) = (eff.tz, eff.ambiguity.clone());
        let jitter_window = eff.window();
        // If an effective timing input changed since this slot was pinned (a
        // `spec.schedule.{timezone,jitter}` edit or a repo `scheduleDefaults` change),
        // the pinned wall-clock instant is stale. Recompute deterministically via the
        // existing `next_fire` (croner + deterministic jitter — NO new randomness)
        // and re-pin with the new inputs without firing; the requeue re-enters and the
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
                    "nextSchedule": eff.pin_json(next),
                    "observedGeneration": generation,
                    "conditions": conditions,
                }),
            )
            .await?;
            tracing::info!(
                schedule = %sched_name,
                from_timezone = ?pinned_tz, to_timezone = %tz.name(),
                from_jitter = ?pinned_jitter, to_jitter = ?eff.jitter,
                "effective cron timing changed (timezone and/or jitter window); \
                 recomputed the pinned slot"
            );
            let until = (next - now).to_std().unwrap_or(StdDuration::from_secs(60));
            return Ok(Action::requeue(until.max(StdDuration::from_secs(1))));
        }
        // Is a run currently active (an unfinished Snapshot owned by this
        // schedule)? Classified from the children slice fetched once above; a
        // list failure fails CLOSED here (propagates), never "no active runs".
        let items = match children {
            Ok(items) => items,
            Err(e) => return Err(e),
        };
        let runs = classify_active_runs(&items);
        let disposition = slot_disposition(&schedule.spec.schedule, slot, now, runs.active);
        // `concurrencyPolicy: Replace` — what to do with the runs this slot is
        // about to replace, decided from the SAME children slice (no extra
        // LIST). Computed BEFORE the blocker filter below because two of its
        // outcomes must convert this Fire into a Wait: `BlockedUnreadable`
        // (fail closed — never delete an unclassifiable run) folds into the
        // very same `ScheduleRunnable` gate `Forbid` uses, and
        // `HeldByParkedRun` holds the slot while the repository's mover-Job
        // concurrency pool is saturated. Exhaustive over the policy so a new
        // variant must state its replacement semantics before this compiles.
        let replace = match schedule.spec.schedule.concurrency_policy {
            ConcurrencyPolicy::Forbid | ConcurrencyPolicy::Allow => None,
            ConcurrencyPolicy::Replace => (disposition == SlotDisposition::Fire && runs.active)
                .then(|| replace_plan(&items, slot)),
        };
        // The two refuse-to-fire plans downgrade the disposition, so the
        // pin is kept and the requeue re-enters (exactly the `Wait` contract).
        let (disposition, parked_hold) = match &replace {
            None | Some(ReplacePlan::Clear | ReplacePlan::Delete(_)) => (disposition, None),
            Some(ReplacePlan::BlockedUnreadable(_)) => (SlotDisposition::Wait, None),
            Some(ReplacePlan::HeldByParkedRun(name)) => {
                (SlotDisposition::Wait, Some(name.as_str()))
            }
        };
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
        // The ONE hold fact every status write below asserts (or clears). The
        // unreadable gate wins when both could apply — `replace_plan` already
        // gives it precedence, so this only re-states that ordering.
        let hold = match (blocker, parked_hold) {
            (Some(b), _) => Some(ScheduleHold::Unreadable(b)),
            (None, Some(p)) => Some(ScheduleHold::ParkedRun(p)),
            (None, None) => None,
        };
        // A slot expired past `startingDeadlineSeconds` must re-pin forward, or the
        // wait branch below computes `(slot - now)` for a past instant and requeues
        // at the 1s floor forever with the pin stuck on the expired slot. Skip it
        // (CronJob missed-slot semantics), record the skip as a Normal event, and
        // pin the next upcoming slot.
        if disposition == SlotDisposition::SkipExpired {
            let next = next_fire(&schedule.spec.schedule.cron, jitter_window, &seed, now, tz)?;
            let (conditions, generation) =
                schedule_ready_status(schedule, tz_ambiguity.as_ref(), hold, None);
            io::patch_status(
                &api,
                &sched_name,
                serde_json::json!({
                    "nextSchedule": eff.pin_json(next),
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
            // `concurrencyPolicy: Replace`: cancel the runs this slot replaces
            // BEFORE minting, so the old mover is stopped rather than left
            // racing the new one. Exhaustive — the two hold variants are
            // unreachable here (they downgraded the disposition to `Wait`
            // above), but they are STATED rather than swept into a catch-all so
            // a future plan variant must decide its fire-time behavior.
            match &replace {
                None | Some(ReplacePlan::Clear) => {}
                Some(ReplacePlan::Delete(victims)) => {
                    replace_active_runs(ctx, schedule, &namespace, &snap_api, victims).await?;
                }
                Some(ReplacePlan::BlockedUnreadable(_) | ReplacePlan::HeldByParkedRun(_)) => {}
            }
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
                hold,
                Some(&outcome.cap_skipped),
            );
            io::patch_status(
                &api,
                &sched_name,
                serde_json::json!({
                    "lastSchedule": { "at": slot.to_rfc3339(), "snapshotRef": snapshot_ref.map(|n| serde_json::json!({ "name": n })) },
                    "nextSchedule": eff.pin_json(next),
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
            let (conditions, generation) = schedule_ready_status(
                schedule,
                tz_ambiguity.as_ref(),
                Some(ScheduleHold::Unreadable(blocked)),
                None,
            );
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
        // the first pass after the blocking Snapshot goes away. The
        // `ReplacementHeld` marker rides along too, in BOTH directions, so
        // entering the hold records it (arming the event's transition guard) and
        // the pool draining clears it.
        //
        // Note this Degraded skip is now reachable for a schedule that sets its own
        // `spec.schedule.timezone`, because resolving the INHERITED jitter half still
        // needs the referent reads. That widens an existing behavior class rather
        // than adding one — the skip already applied to every inheriting schedule —
        // and it is self-correcting: these conditions are re-evaluated on the next
        // successful pass (the pin's own requeue, or the referent watch firing when
        // the policy/repository returns), so a stale `TimezoneDefaultAmbiguous` or
        // `ScheduleRunnable` lingers at most until then and never past the next fire.
        let held_recorded = recorded_held_by_parked_run(schedule);
        let held_now = parked_hold.is_some();
        if matches!(resolution, ScheduleResolution::Resolved { .. })
            && (tz_ambiguity.is_some() != recorded_tz_ambiguous(schedule)
                || recorded_blocked_on_unreadable(schedule)
                || held_now != held_recorded)
        {
            let (conditions, generation) =
                schedule_ready_status(schedule, tz_ambiguity.as_ref(), hold, None);
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
        // `Replace` is declining to fire a DUE slot. That is benign and
        // self-clearing (unlike the unreadable hold above), so it is a Normal
        // event rather than a Warning + structural gate — but it must not be
        // INVISIBLE: a schedule that silently stops firing is the #359 shape,
        // and a debug log alone leaves an operator watching `kubectl get
        // snapshots` with no explanation. Transition-gated on the condition just
        // written, so it fires once on entering the hold, not once per 1s requeue.
        if let Some(parked) = parked_hold {
            if !held_recorded {
                io::publish_normal_event(
                    ctx,
                    schedule,
                    kopiur_api::consts::WAITING_FOR_REPOSITORY_SLOT_REASON,
                    "AwaitRepositorySlot",
                    &format!(
                        "concurrencyPolicy: Replace is holding this slot: the run it would \
                         replace (`{parked}`) is itself queued behind its repository's mover-Job \
                         concurrency cap, so cancelling it would free no capacity. Backups \
                         resume automatically when the pool drains; raise the repository's \
                         mover concurrency if this persists."
                    ),
                )
                .await;
            }
            tracing::debug!(
                namespace = %namespace, schedule = %sched_name, parked_run = %parked,
                "concurrencyPolicy: Replace is holding this slot behind the repository's \
                 mover-Job concurrency cap"
            );
        }
        let until = (slot - now).to_std().unwrap_or(StdDuration::from_secs(1));
        return Ok(Action::requeue(until.max(StdDuration::from_secs(1))));
    }

    // First reconcile (nextSchedule not yet pinned). Choose the timing inputs to pin:
    // the resolved zone/window, or UTC with no jitter when Degraded (self-heals —
    // once referents recover, the pinned-slot branch recomputes into the inherited
    // zone/window exactly once).
    let eff = first_pin(&resolution);
    let (tz, tz_ambiguity) = (eff.tz, eff.ambiguity.clone());
    let next = next_fire(&schedule.spec.schedule.cron, eff.window(), &seed, now, tz)?;

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
                "nextSchedule": eff.pin_json(next),
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
            "nextSchedule": eff.pin_json(next),
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

/// What `concurrencyPolicy: Replace` must do with a due slot, given this
/// schedule's children. Four outcomes, because "cancel the old one" has two
/// distinct ways of being the wrong move — and both must stop the fire, not
/// just skip the deletes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplacePlan {
    /// Nothing unfinished is in flight — fire normally, delete nothing.
    Clear,
    /// Delete exactly these children (sorted, so a fire is deterministic
    /// regardless of LIST/store order), then fire.
    Delete(Vec<String>),
    /// A child sits at a phase THIS build cannot read. Fail closed exactly like
    /// [`classify_active_runs`]: never delete what cannot be classified — the
    /// run may be live, written by a newer operator. Handled like `Forbid`'s
    /// hold (the `ScheduleRunnable=False` gate), so the operator sees the wedge
    /// instead of `Replace` silently destroying an in-flight newer-operator run.
    BlockedUnreadable(UnreadableRun),
    /// A child is parked behind its repository's mover-Job concurrency cap
    /// ([`kopiur_api::consts::REPOSITORY_SLOT_AVAILABLE_CONDITION`] = `False`),
    /// carrying its name. Such a child is QUEUED, not running: deleting it
    /// would free nothing and the replacement would immediately park in the
    /// same queue — a delete-mint-park livelock that burns a CR per slot and
    /// never makes progress — while minting a sibling beside it is precisely
    /// the pileup the cap exists to prevent. So `Replace` degrades to
    /// `Forbid`-like behavior while the pool is saturated: no deletes, no fire,
    /// wait for the slot. Self-clears the moment the pool drains.
    HeldByParkedRun(String),
}

/// **Pure.** Whether `snapshot` was created strictly BEFORE `slot` — i.e. it
/// belongs to an earlier slot and is therefore a legitimate replacement victim.
/// A missing `creationTimestamp` answers `false` (fail closed: an undatable row
/// is never cancelled). See [`replace_plan`] for why this cut is exact.
fn created_before_slot(snapshot: &Snapshot, slot: DateTime<Utc>) -> bool {
    snapshot
        .creation_timestamp()
        .and_then(|t| DateTime::<Utc>::from_timestamp(t.0.as_second(), 0))
        .is_some_and(|created| created < slot)
}

/// **Pure.** Whether a `Snapshot` is parked behind its repository's mover-Job
/// concurrency cap — i.e. it holds
/// [`REPOSITORY_SLOT_AVAILABLE_CONDITION`](kopiur_api::consts::REPOSITORY_SLOT_AVAILABLE_CONDITION)
/// at `False`. Callers scope this to the UNFINISHED, non-terminating children:
/// a long-finished run whose last recorded conditions still carry a stale
/// `False` must never hold a schedule shut forever.
fn parked_behind_slot(snapshot: &Snapshot) -> bool {
    snapshot
        .status
        .as_ref()
        .map(|s| s.conditions.as_slice())
        .unwrap_or_default()
        .iter()
        .any(|c| {
            c.type_ == kopiur_api::consts::REPOSITORY_SLOT_AVAILABLE_CONDITION
                && c.status == "False"
        })
}

/// **Pure.** Decide what `concurrencyPolicy: Replace` does with a due slot,
/// from this schedule's `Snapshot` children. Split from the IO so the whole
/// truth table — including both refuse-to-fire cases — is unit-tested without
/// a cluster, exactly like [`classify_active_runs`] beside it.
///
/// "Unfinished" is the SAME set the concurrency gate uses (`None` / `Pending` /
/// `Running`), matched exhaustively so a new phase must state which side of the
/// replacement it falls on before this compiles. Rows already terminating
/// (`deletionTimestamp` set) are skipped — they are on their way out and
/// re-deleting them is a no-op that would only pad the event.
///
/// **Precedence is fail-closed first.** An `Unknown` phase short-circuits to
/// [`ReplacePlan::BlockedUnreadable`] even when known-unfinished (or parked)
/// children are also present: the unreadable row is the one that needs a human,
/// and deleting its siblings while it is un-classifiable would be acting on a
/// half-understood picture. A parked child then wins over the delete set for
/// the livelock reason in [`ReplacePlan::HeldByParkedRun`].
///
/// ## `slot` is a data-safety filter, not a convenience
///
/// `Replace` must only ever cancel runs from a PREVIOUS slot. Without this
/// filter a **retried** fire silently eats its own output: if the first attempt
/// minted the child and then failed before the status patch landed (a mid
/// fan-out error, or a 409 on the status patch), the pin is still un-advanced,
/// so the retry re-enters with `disposition == Fire`, sees its own brand-new
/// `Pending` child as "an in-flight run", kills that child's mover Job, deletes
/// the CR — and then [`slot_fire_blocked_by_terminating`] skips the re-fire
/// because the slot twin is terminating. The eventual status patch still stamps
/// the slot as fired. Net effect: the slot is recorded as run and **no backup
/// exists**. Silent data loss, once per retried fire.
///
/// The cut is `creationTimestamp >= slot` and it is exact, not heuristic: a fire
/// re-pins `nextSchedule` via `next_fire(.., after = now, ..)`, which is always
/// strictly LATER than the `now` at which that fire minted its children (cron
/// occurrences are minute-granular and jitter only adds). So every child of an
/// earlier slot was created strictly before the currently-pinned `slot`
/// instant, and every child of THIS slot at or after it — including a
/// late-firing catch-up slot, whose successor pin is computed from `now`, not
/// from the slot.
///
/// A child with **no** `creationTimestamp` is skipped too (fail closed): an
/// undatable row cannot be proven to belong to a previous slot, and the cost of
/// being wrong is asymmetric — wrongly skipping degrades `Replace` to `Allow`
/// for one slot, wrongly deleting destroys a run.
pub fn replace_plan(items: &[Snapshot], slot: DateTime<Utc>) -> ReplacePlan {
    use kopiur_api::SnapshotPhase;
    let mut victims: Vec<String> = Vec::new();
    let mut parked: Option<String> = None;
    for b in items {
        if b.metadata.deletion_timestamp.is_some() {
            continue;
        }
        // Never cancel this slot's own output (see the doc above). Applied
        // BEFORE the phase match so a retry's own child cannot even raise the
        // unreadable/parked holds — it is not a "previous run" at all, and the
        // re-fire's server-side apply converges on it idempotently.
        if !created_before_slot(b, slot) {
            continue;
        }
        let unfinished = match b.status.as_ref().and_then(|s| s.phase.as_ref()) {
            None | Some(SnapshotPhase::Pending | SnapshotPhase::Running) => true,
            Some(
                SnapshotPhase::Succeeded
                | SnapshotPhase::Failed
                | SnapshotPhase::Deleting
                | SnapshotPhase::Discovered
                | SnapshotPhase::Unchanged,
            ) => false,
            // Fail CLOSED, and louder than every other outcome: this build
            // cannot tell whether the run is live, so it must neither delete it
            // nor mint beside it. Reports the FIRST such child (matching
            // `classify_active_runs`) so the two gates name the same object.
            Some(SnapshotPhase::Unknown(raw)) => {
                return ReplacePlan::BlockedUnreadable(UnreadableRun {
                    snapshot: b.name_any(),
                    phase: raw.clone(),
                });
            }
        };
        if !unfinished {
            continue;
        }
        if parked.is_none() && parked_behind_slot(b) {
            parked = Some(b.name_any());
        }
        victims.push(b.name_any());
    }
    if let Some(name) = parked {
        return ReplacePlan::HeldByParkedRun(name);
    }
    if victims.is_empty() {
        return ReplacePlan::Clear;
    }
    victims.sort();
    ReplacePlan::Delete(victims)
}

/// **Pure.** Whether a victim read LIVE is still cancellable: it must exist, not
/// already be terminating, and still be unfinished (`None` / `Pending` /
/// `Running` — the same set [`replace_plan`] selects on, matched exhaustively so
/// a new phase must state its answer here too).
///
/// The phase re-check is the point. `replace_plan` runs against a possibly
/// store-derived slice, and a `Running` victim can commit its kopia snapshot and
/// flip to `Succeeded` in the window between that selection and this delete.
/// Existence-and-not-terminating alone would happily cancel it — deleting the CR
/// of a **completed** backup and leaving its kopia snapshot unreferenced. This
/// collapses that window to the sub-millisecond gap between this GET and the
/// delete below.
///
/// `Unknown` answers `false` for the usual reason: never destroy a run whose
/// phase this build cannot interpret.
fn still_replaceable(live: Option<&Snapshot>) -> bool {
    use kopiur_api::SnapshotPhase;
    let Some(row) = live else {
        return false;
    };
    if row.metadata.deletion_timestamp.is_some() {
        return false;
    }
    match row.status.as_ref().and_then(|s| s.phase.as_ref()) {
        None | Some(SnapshotPhase::Pending | SnapshotPhase::Running) => true,
        Some(
            SnapshotPhase::Succeeded
            | SnapshotPhase::Failed
            | SnapshotPhase::Deleting
            | SnapshotPhase::Discovered
            | SnapshotPhase::Unchanged
            | SnapshotPhase::Unknown(_),
        ) => false,
    }
}

/// Execute a [`ReplacePlan::Delete`]: cancel each victim run, then report the
/// names actually removed (a victim the live read says is gone, terminating, or
/// already finished is skipped and never appears in the Event).
///
/// Per victim, in this order:
///
/// 1. **Live-verify** (#382 C2): `children` may be reflector-store-derived —
///    trust the cache to SELECT, verify live before DESTROYING. A row that
///    vanished, started terminating, **or has since finished** is skipped, so a
///    stale store cannot re-delete an already-replaced run nor cancel a run that
///    completed after the snapshot was taken ([`still_replaceable`]).
/// 2. **Delete the mover Job first.** The Job carries the SAME name as its
///    `Snapshot` (see `snapshot::launch`'s `apply_mover_objects`). This is a
///    BACKGROUND delete (matching every other `delete_mover_run` caller), so the
///    pod is reaped asynchronously rather than synchronously — the guarantee is
///    ordering, not instantaneous death. What it buys is that the Job is
///    explicitly targeted *before* the CR goes away: deleting the CR first would
///    leave the mover's teardown entirely to ownerRef GC, and the `Snapshot`
///    finalizer releases immediately while `status.snapshot` is absent (the
///    normal mid-run state), so the CR can be fully gone with nothing having
///    asked the mover to stop.
/// 3. **Stamp then delete the CR** with [`PrunedBy::ReplacedRun`], so the
///    finalizer classifies this as an OPERATOR prune. A bare delete would
///    classify EXTERNAL and every `Replace` fire would push the repository's
///    mass-deletion breaker toward tripping.
///
/// Every step is 404-tolerant (a victim that vanished mid-loop is a no-op
/// success); a non-404 failure propagates so the reconcile retries WITHOUT
/// having minted the replacement.
///
/// Note what this does **not** reclaim: a killed mid-run mover may have left
/// partial upload data (and, if it checkpointed, an incomplete manifest) in the
/// repository. There is no committed kopia snapshot (`status.snapshot` is
/// unset), so the finalizer has nothing to delete; those blobs are reclaimed by
/// kopia's blob garbage collection during `Maintenance`, not here.
async fn replace_active_runs(
    ctx: &Context,
    schedule: &SnapshotSchedule,
    namespace: &str,
    api: &Api<Snapshot>,
    victims: &[String],
) -> Result<Vec<String>> {
    let mut deleted: Vec<String> = Vec::new();
    for name in victims {
        let live = api.get_opt(name).await?;
        if !still_replaceable(live.as_ref()) {
            tracing::debug!(
                schedule = %schedule.name_any(), snapshot = %name,
                "skipping replacement victim: gone, terminating, or no longer in flight on live verify"
            );
            continue;
        }
        io::delete_mover_run(&ctx.client, namespace, name).await?;
        io::annotate_then_delete_snapshot(api, name, PrunedBy::ReplacedRun).await?;
        tracing::info!(
            schedule = %schedule.name_any(), snapshot = %name,
            "cancelled an in-flight run (concurrencyPolicy: Replace)"
        );
        deleted.push(name.clone());
    }
    // ONE Event per fire listing every name — a per-victim Event would multiply
    // a fan-out schedule's slot into an event storm, and this is a single
    // transition (this slot replaced those runs), not N independent facts.
    if !deleted.is_empty() {
        io::publish_normal_event(
            ctx,
            schedule,
            "ReplacedActiveRun",
            "ReplaceInFlightRun",
            &format!(
                "concurrencyPolicy: Replace — cancelled {} in-flight run(s) so this slot could \
                 take their place: {}. Their mover Jobs were deleted; no committed kopia \
                 snapshot existed to reclaim.",
                deleted.len(),
                deleted.join(", ")
            ),
        )
        .await;
    }
    Ok(deleted)
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

/// Resolve the effective cron timing inputs for this reconcile — the timezone (see
/// [`effective_timezone`]) AND the deterministic jitter window (see
/// [`resolve_schedule_jitter`]).
///
/// Each half is independent: `spec.schedule.timezone` / `spec.schedule.jitter` wins
/// for its own half with no inheritance. Lookups are skipped entirely only when BOTH
/// are set — an explicit timezone alone can't answer where the jitter window comes
/// from. Otherwise it GETs each target policy and resolves that policy's
/// repositories' `scheduleDefaults`, then applies agree-or-UTC to the timezones and
/// agree-or-no-jitter to the windows.
///
/// A jitter disagreement is warn-logged here rather than surfaced as a condition
/// (deliberately asymmetric with timezone): "no jitter" is a benign, correct
/// schedule, whereas the wrong timezone silently moves backups by hours.
///
/// Returns [`ScheduleResolution::Degraded`] on any referent GET/list failure (or a
/// missing policy/repo) rather than a genuinely-resolved UTC/no-jitter: a missing or
/// unreadable referent must not wedge scheduling, and — critically — must not be
/// mistaken for real values and used to invalidate an established pin (see
/// [`resolve_pinned_slot`]). The pin's own requeue plus the referent watch recover
/// once the referent returns.
///
/// Note an *empty* matched-policy set (a selector that matches nothing) is a genuine
/// `Resolved { tz: UTC, jitter: None }`, not `Degraded` — there was nothing to
/// inherit, and pinning that is correct.
async fn resolve_effective_schedule(
    ctx: &Context,
    schedule: &SnapshotSchedule,
    namespace: &str,
) -> ScheduleResolution {
    let own_tz = schedule.spec.schedule.timezone.as_deref();
    let own_jitter = schedule.spec.schedule.jitter.as_deref();
    // Both halves set explicitly ⇒ nothing to inherit, so skip the referent lookup.
    if let (Some(tz), Some(jitter)) = (own_tz, own_jitter) {
        return ScheduleResolution::Resolved {
            tz: resolve_tz(Some(tz)),
            ambiguity: None,
            jitter: Some(jitter.to_string()),
        };
    }
    let Some(defaults) = matched_repo_schedule_defaults(ctx, schedule, namespace).await else {
        // Only the INHERITED halves are unknown: an explicitly-set timezone or
        // jitter needed no lookup, so it rides through the degrade rather than
        // collapsing to UTC / un-jittered.
        return ScheduleResolution::Degraded {
            own_tz: own_tz.map(|t| resolve_tz(Some(t))),
            own_jitter: own_jitter.map(str::to_string),
        };
    };
    let (tz, ambiguity) = inherited_timezone(own_tz, &defaults);
    // Exhaustive: a disagreement is a decision (apply no jitter AND tell the
    // operator), not the same fact as "everyone agreed on no jitter".
    let jitter = match inherited_jitter(own_jitter, &defaults) {
        ScheduleJitterResolution::Agreed(window) => window,
        ScheduleJitterResolution::Disagreed { candidates } => {
            tracing::warn!(
                schedule = %schedule.name_any(),
                namespace = %namespace,
                candidates = %candidates.join(", "),
                "matched policies' repositories disagree on scheduleDefaults.jitter; \
                 applying NO jitter — set spec.schedule.jitter explicitly to choose a window"
            );
            None
        }
    };
    ScheduleResolution::Resolved {
        tz,
        ambiguity,
        jitter,
    }
}

/// Gather one [`ScheduleDefaults`] per repository of every matched target policy
/// (the same target set the fire path uses — a single `policyRef`, or each
/// `policySelector` match). One entry PER REPOSITORY (#368): a multi-repo policy's
/// members feed the same agree-or-UTC / agree-or-no-jitter kernels as cross-policy
/// defaults do, so within-policy disagreement resolves identically.
///
/// `None` means the pass is **degraded** — a referent GET/list failed, or a matched
/// policy/repository was missing. The reason is logged here because the caller only
/// needs the yes/no: it maps `None` straight onto [`ScheduleResolution::Degraded`],
/// which preserves an established pin rather than mistaking a failure for a genuine
/// "UTC, no jitter" resolution.
async fn matched_repo_schedule_defaults(
    ctx: &Context,
    schedule: &SnapshotSchedule,
    namespace: &str,
) -> Option<Vec<ScheduleDefaults>> {
    let policy_refs = match target_policy_refs(ctx, schedule, namespace).await {
        Ok(refs) => refs,
        Err(e) => {
            tracing::debug!(error = %e, "listing target policies for schedule defaults failed; degrading (established pin preserved)");
            return None;
        }
    };
    let mut defaults: Vec<ScheduleDefaults> = Vec::with_capacity(policy_refs.len());
    for pref in &policy_refs {
        match policy_repo_schedule_defaults(ctx, pref, namespace).await {
            Ok(per_repo) => defaults.extend(per_repo),
            Err(e) => {
                tracing::debug!(policy = %pref.name, error = %e, "resolving policy repository schedule defaults failed; degrading (established pin preserved)");
                return None;
            }
        }
    }
    Some(defaults)
}

/// **Pure.** Project the matched repositories' `scheduleDefaults` onto the timezone
/// half and run the agree-or-UTC kernel. An explicit `spec.schedule.timezone` (`own`)
/// wins outright and is never ambiguous — the lookups happened only because the
/// jitter half still needed them.
fn inherited_timezone(
    own: Option<&str>,
    defaults: &[ScheduleDefaults],
) -> (Tz, Option<TimezoneAmbiguity>) {
    if let Some(own) = own {
        return (resolve_tz(Some(own)), None);
    }
    let zones: Vec<Option<String>> = defaults.iter().map(|d| d.timezone.clone()).collect();
    effective_timezone(None, &zones)
}

/// **Pure.** The jitter half of [`inherited_timezone`]: project the matched
/// repositories' `scheduleDefaults` onto the jitter window and run the
/// agree-or-no-jitter kernel, preserving the disagreement signal for the caller's
/// warn (see [`ScheduleJitterResolution`]).
fn inherited_jitter(own: Option<&str>, defaults: &[ScheduleDefaults]) -> ScheduleJitterResolution {
    if let Some(own) = own {
        return ScheduleJitterResolution::Agreed(Some(own.to_string()));
    }
    let windows: Vec<Option<String>> = defaults.iter().map(|d| d.jitter.clone()).collect();
    resolve_schedule_jitter(None, &windows)
}

/// GET one target policy and return its repository/-ies' `scheduleDefaults`, one
/// entry PER REPOSITORY (#368 audit M9: a multi-repo policy contributes each
/// member's defaults, so the entries flow through the SAME agree-or-UTC + ambiguity
/// kernel ([`effective_timezone`]) — and the same agree-or-no-jitter kernel
/// ([`resolve_schedule_jitter`]) — as cross-policy defaults do; all members agree ⇒
/// that value; disagreement ⇒ UTC + the existing `TimezoneDefaultAmbiguous` warning
/// condition (timezone) or no jitter + a warn log (jitter), deterministically, with
/// no new admission machinery). The single-repo shape returns exactly one entry —
/// behavior unchanged. Honors `policyRef.namespace` for a cross-namespace ref; the
/// policy's repositories resolve in the policy's own namespace (matching how the
/// policy itself resolves them). A repository that sets no `scheduleDefaults` at all
/// contributes the empty default (every field `None`), which is exactly "sets no
/// default" for each half.
async fn policy_repo_schedule_defaults(
    ctx: &Context,
    policy_ref: &PolicyRef,
    schedule_ns: &str,
) -> Result<Vec<ScheduleDefaults>> {
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
        defaults.push(repo.schedule_defaults.unwrap_or_default());
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

    /// A pin recording only a timezone — the pre-jitter shape.
    fn pinned_tz_only(tz: &str) -> PinnedSlot<'_> {
        PinnedSlot {
            timezone: Some(tz),
            jitter: None,
        }
    }

    #[test]
    fn pin_recompute_only_when_recorded_zone_differs() {
        // Differing recorded zone → recompute (the pin is stale).
        assert!(pin_needs_recompute(
            pinned_tz_only("America/Chicago"),
            Tz::UTC,
            None
        ));
        assert!(pin_needs_recompute(
            pinned_tz_only("UTC"),
            "Europe/Berlin".parse().unwrap(),
            None
        ));
    }

    #[test]
    fn pin_recompute_equal_zone_keeps_pin_no_churn() {
        // Determinism guard: identical zones never recompute (no jitter churn).
        assert!(!pin_needs_recompute(pinned_tz_only("UTC"), Tz::UTC, None));
        assert!(!pin_needs_recompute(
            pinned_tz_only("America/Chicago"),
            "America/Chicago".parse().unwrap(),
            None
        ));
    }

    #[test]
    fn pin_recompute_legacy_absent_zone_keeps_pin() {
        // A legacy pin (no recorded zone) is treated as unchanged — no upgrade churn.
        assert!(!pin_needs_recompute(PinnedSlot::default(), Tz::UTC, None));
        assert!(!pin_needs_recompute(
            PinnedSlot::default(),
            "Pacific/Kiritimati".parse().unwrap(),
            None
        ));
    }

    #[test]
    fn pin_recompute_when_the_recorded_jitter_window_differs() {
        // A repo `scheduleDefaults.jitter` edit changes the inherited window with no
        // edit to the SnapshotSchedule at all — the pin must go stale, or the change
        // would only take effect an arbitrary slot later.
        let pinned = PinnedSlot {
            timezone: Some("UTC"),
            jitter: Some("10m"),
        };
        assert!(
            pin_needs_recompute(pinned, Tz::UTC, Some("30m")),
            "a widened repo default must invalidate the pin"
        );
        assert!(
            pin_needs_recompute(pinned, Tz::UTC, None),
            "a REMOVED repo default must invalidate the pin too"
        );
        assert!(
            !pin_needs_recompute(pinned, Tz::UTC, Some("10m")),
            "the unchanged window is the steady state — never recompute"
        );
    }

    #[test]
    fn pin_recompute_compares_the_parsed_window_not_the_spelling() {
        // `60m` and `1h` are the same window; a recompute here would be pure churn
        // AND can skip a slot (it re-anchors `next_fire` at `now`).
        let pinned = PinnedSlot {
            timezone: Some("UTC"),
            jitter: Some("60m"),
        };
        assert!(!pin_needs_recompute(pinned, Tz::UTC, Some("1h")));
    }

    #[test]
    fn pin_recompute_absent_recorded_jitter_keeps_pin() {
        // Upgrade-churn rule, jitter half: a pin written before the field existed
        // records no window, so an inherited window must NOT retroactively
        // invalidate every pin in the cluster on the operator upgrade. It is picked
        // up at the next natural re-pin (fire / expiry).
        assert!(!pin_needs_recompute(
            pinned_tz_only("UTC"),
            Tz::UTC,
            Some("30m")
        ));
        assert!(!pin_needs_recompute(
            PinnedSlot::default(),
            Tz::UTC,
            Some("30m")
        ));
    }

    #[test]
    fn pin_recompute_is_the_or_of_both_timing_inputs() {
        // Either input alone invalidates; neither leaves the pin alone. Stated as a
        // truth table so a third timing input can't be added and silently ignored.
        let pinned = PinnedSlot {
            timezone: Some("UTC"),
            jitter: Some("10m"),
        };
        let berlin: Tz = "Europe/Berlin".parse().unwrap();
        assert!(!pin_needs_recompute(pinned, Tz::UTC, Some("10m")));
        assert!(pin_needs_recompute(pinned, berlin, Some("10m")));
        assert!(pin_needs_recompute(pinned, Tz::UTC, Some("20m")));
        assert!(pin_needs_recompute(pinned, berlin, Some("20m")));
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

    fn resolved(name: &str) -> ScheduleResolution {
        ScheduleResolution::Resolved {
            tz: name.parse().unwrap(),
            ambiguity: None,
            jitter: None,
        }
    }

    /// A degraded pass for a schedule that sets NEITHER timing input itself — both
    /// halves were inherited, so both are unknown.
    fn degraded_inherited() -> ScheduleResolution {
        ScheduleResolution::Degraded {
            own_tz: None,
            own_jitter: None,
        }
    }

    /// A degraded pass for a schedule that set the halves named here itself.
    fn degraded_own(tz: Option<&str>, jitter: Option<&str>) -> ScheduleResolution {
        ScheduleResolution::Degraded {
            own_tz: tz.map(|t| t.parse().unwrap()),
            own_jitter: jitter.map(str::to_string),
        }
    }

    fn resolved_with_jitter(name: &str, jitter: Option<&str>) -> ScheduleResolution {
        ScheduleResolution::Resolved {
            tz: name.parse().unwrap(),
            ambiguity: None,
            jitter: jitter.map(str::to_string),
        }
    }

    #[test]
    fn degraded_keeps_established_non_utc_pin() {
        // REGRESSION (reviewer's flap concern): an established Europe/Berlin pin must
        // NOT be invalidated when timezone resolution degrades (a transient referent
        // failure). On the old `(Tz::UTC, None)`-on-failure code the caller could not
        // tell this from a resolved UTC, so `pin_needs_recompute(Some("Europe/Berlin"),
        // UTC)` fired and rewrote the pin to UTC timing — then flapped back on recovery.
        let (eff, needs_recompute) =
            resolve_pinned_slot(pinned_tz_only("Europe/Berlin"), &degraded_inherited());
        assert!(!needs_recompute, "degrade must never invalidate a live pin");
        // The pin's own recorded zone stays in effect for this reconcile (no flap).
        assert_eq!(eff.tz.name(), "Europe/Berlin");
        assert!(eff.ambiguity.is_none());
    }

    #[test]
    fn degraded_keeps_the_established_pins_jitter_window_too() {
        // The jitter half of the same invariant: a repo GET blip must not silently
        // drop an inherited window and re-pin the slot un-jittered.
        let pinned = PinnedSlot {
            timezone: Some("Europe/Berlin"),
            jitter: Some("30m"),
        };
        let (eff, needs_recompute) = resolve_pinned_slot(pinned, &degraded_inherited());
        assert!(!needs_recompute, "degrade must never invalidate a live pin");
        assert_eq!(eff.tz.name(), "Europe/Berlin");
        assert_eq!(eff.jitter.as_deref(), Some("30m"));
        assert_eq!(eff.window(), Some(StdDuration::from_secs(1800)));
        // And it re-pins the SAME values it read, so a degraded pass that happens to
        // write status (a Fire, say) cannot rewrite the pin's timing inputs.
        let pin = eff.pin_json(at(2026, 5, 24, 2, 0));
        assert_eq!(pin["timezone"], "Europe/Berlin");
        assert_eq!(pin["jitter"], "30m");
    }

    #[test]
    fn resolved_differing_zone_invalidates_established_pin() {
        // A genuine resolution to a different zone still recomputes (the pin is stale).
        let (eff, needs_recompute) =
            resolve_pinned_slot(pinned_tz_only("UTC"), &resolved("Europe/Berlin"));
        assert!(needs_recompute);
        assert_eq!(eff.tz.name(), "Europe/Berlin");
        // Same zone resolved → no churn.
        let (_eff, again) =
            resolve_pinned_slot(pinned_tz_only("Europe/Berlin"), &resolved("Europe/Berlin"));
        assert!(!again);
    }

    #[test]
    fn a_changed_repo_default_jitter_invalidates_the_pin_and_re_pins_the_new_window() {
        // The end-to-end pin-invalidation contract for the jitter half: pinned at
        // 10m, the repository's `scheduleDefaults.jitter` is edited to 30m, the pin
        // is stale, and the value written back is the NEW window (not the old one).
        let pinned = PinnedSlot {
            timezone: Some("UTC"),
            jitter: Some("10m"),
        };
        let (eff, needs_recompute) =
            resolve_pinned_slot(pinned, &resolved_with_jitter("UTC", Some("30m")));
        assert!(needs_recompute, "the inherited window changed");
        assert_eq!(eff.jitter.as_deref(), Some("30m"));
        assert_eq!(eff.window(), Some(StdDuration::from_secs(1800)));
        assert_eq!(eff.pin_json(at(2026, 5, 24, 2, 0))["jitter"], "30m");

        // ...and the re-pinned state is stable (no churn on the next pass).
        let repinned = PinnedSlot {
            timezone: Some("UTC"),
            jitter: Some("30m"),
        };
        let (_eff, again) =
            resolve_pinned_slot(repinned, &resolved_with_jitter("UTC", Some("30m")));
        assert!(!again, "must stabilize after the one re-pin");
    }

    #[test]
    fn a_removed_repo_default_jitter_re_pins_with_the_window_cleared() {
        // Dropping `scheduleDefaults.jitter` must clear the recorded window, not
        // leave a stale one behind to invalidate every future pin forever.
        let pinned = PinnedSlot {
            timezone: Some("UTC"),
            jitter: Some("10m"),
        };
        let (eff, needs_recompute) = resolve_pinned_slot(pinned, &resolved("UTC"));
        assert!(needs_recompute);
        assert!(eff.jitter.is_none());
        assert_eq!(eff.window(), None);
        // JSON `null` under a merge patch DELETES the recorded window.
        assert_eq!(
            eff.pin_json(at(2026, 5, 24, 2, 0))["jitter"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn first_pin_degrade_then_recover_recomputes_exactly_once() {
        // (1) First reconcile while degraded: self-heal by pinning UTC now.
        let eff0 = first_pin(&degraded_inherited());
        assert_eq!(eff0.tz.name(), "UTC");
        assert!(eff0.ambiguity.is_none());
        assert!(eff0.jitter.is_none(), "degraded self-heal pins no jitter");
        let pinned_tz = eff0.tz.name().to_string(); // recorded on the pin = "UTC"

        // (2) Referents recover and resolve to the inherited Europe/Berlin: the
        // pinned-slot branch recomputes exactly once (UTC != Europe/Berlin).
        let (eff1, recompute1) =
            resolve_pinned_slot(pinned_tz_only(&pinned_tz), &resolved("Europe/Berlin"));
        assert!(
            recompute1,
            "recovery must recompute the UTC self-heal pin once"
        );
        assert_eq!(eff1.tz.name(), "Europe/Berlin");
        let pinned_tz = eff1.tz.name().to_string(); // re-pinned as Europe/Berlin

        // (3) Steady state: the same resolution no longer recomputes (stabilizes).
        let (_eff2, recompute2) =
            resolve_pinned_slot(pinned_tz_only(&pinned_tz), &resolved("Europe/Berlin"));
        assert!(
            !recompute2,
            "must stabilize — no repeated churn after recovery"
        );
    }

    /// **REGRESSION for the lookup this task introduced.** Resolving the jitter half
    /// now requires referent reads even when `spec.schedule.timezone` IS set — so a
    /// missing/unreadable policy can degrade a pass that previously never could. The
    /// explicitly-written zone must survive that: it needed no lookup.
    #[test]
    fn a_degraded_pass_keeps_an_explicitly_set_own_timezone() {
        let chicago: Tz = "America/Chicago".parse().unwrap();
        let degraded = degraded_own(Some("America/Chicago"), None);
        // First pin: the user's zone, NOT the UTC self-heal.
        let eff = first_pin(&degraded);
        assert_eq!(eff.tz, chicago);
        assert!(eff.jitter.is_none(), "the inherited half is still unknown");

        // Established pin in the same zone: nothing changed, no churn.
        let (eff, recompute) = resolve_pinned_slot(pinned_tz_only("America/Chicago"), &degraded);
        assert_eq!(eff.tz, chicago);
        assert!(!recompute);

        // An explicit timezone EDIT still invalidates the pin while degraded —
        // exactly as it did before this half needed lookups at all.
        let (eff, recompute) = resolve_pinned_slot(pinned_tz_only("UTC"), &degraded);
        assert!(
            recompute,
            "an own-timezone edit must not wait on an unrelated referent"
        );
        assert_eq!(eff.tz, chicago);
    }

    /// **REGRESSION, the jitter mirror — and the sharper half.** A schedule that sets
    /// `spec.schedule.jitter` but NOT `timezone` still needs referent reads (for the
    /// timezone half), so a routine GitOps bundle-apply ordering — policy/repository
    /// momentarily absent — degrades its FIRST pin.
    ///
    /// If that first pin recorded no window, nothing ever puts one back:
    /// `pin_needs_recompute`'s jitter arm is `is_some_and` on the PINNED side, so an
    /// absent recorded window is "unchanged" forever. The schedule would fire
    /// un-jittered permanently — the exact stampede jitter exists to prevent — while
    /// every object involved looks healthy.
    #[test]
    fn a_degraded_first_pin_keeps_an_explicitly_set_own_jitter() {
        let degraded = degraded_own(None, Some("10m"));
        let eff = first_pin(&degraded);
        assert_eq!(
            eff.jitter.as_deref(),
            Some("10m"),
            "an own window needs no lookup, so a degraded pass must still pin it"
        );
        assert_eq!(eff.window(), Some(StdDuration::from_secs(600)));
        assert_eq!(
            eff.tz,
            Tz::UTC,
            "the INHERITED half still self-heals to UTC"
        );
        // The pin actually written carries the window — this is what makes the
        // recovery below a no-op instead of a permanent un-jittered slot.
        assert_eq!(eff.pin_json(at(2026, 5, 24, 2, 0))["jitter"], "10m");
    }

    #[test]
    fn a_degraded_first_pin_with_own_jitter_recovers_without_churn() {
        // (1) Degraded first pin records the own window.
        let eff0 = first_pin(&degraded_own(None, Some("10m")));
        let pinned = PinnedSlot {
            timezone: Some(eff0.tz.name()),
            jitter: eff0.jitter.as_deref(),
        };

        // (2) Referents recover. Own jitter still wins, so the resolved window is the
        // same one already pinned: the jitter half contributes NO recompute.
        let (eff1, recompute) =
            resolve_pinned_slot(pinned, &resolved_with_jitter("UTC", Some("10m")));
        assert!(!recompute, "the pinned window already matches — no churn");
        assert_eq!(eff1.jitter.as_deref(), Some("10m"));

        // (3) And when the inherited TIMEZONE half also recovers to something else,
        // the one recompute it drives re-pins the window unchanged rather than
        // dropping it.
        let (eff2, recompute) =
            resolve_pinned_slot(pinned, &resolved_with_jitter("Europe/Berlin", Some("10m")));
        assert!(recompute, "driven by the recovered timezone");
        assert_eq!(eff2.tz.name(), "Europe/Berlin");
        assert_eq!(eff2.jitter.as_deref(), Some("10m"));
    }

    #[test]
    fn a_degraded_pass_keeps_an_explicitly_set_own_jitter_on_an_established_pin() {
        // The established-pin mirror: an own-jitter EDIT still invalidates while
        // degraded (it needed no lookup), and the re-pin records the NEW window.
        let degraded = degraded_own(None, Some("10m"));
        let pinned = PinnedSlot {
            timezone: Some("UTC"),
            jitter: Some("30m"),
        };
        let (eff, recompute) = resolve_pinned_slot(pinned, &degraded);
        assert!(
            recompute,
            "an own-jitter edit must not wait on an unrelated referent"
        );
        assert_eq!(eff.jitter.as_deref(), Some("10m"));
        // Unchanged own jitter → no churn.
        let same = PinnedSlot {
            timezone: Some("UTC"),
            jitter: Some("10m"),
        };
        let (_eff, recompute) = resolve_pinned_slot(same, &degraded);
        assert!(!recompute);
    }

    /// Both halves at once, degraded: each is authoritative, and the timezone half
    /// no longer self-heals to UTC because it was explicitly set.
    #[test]
    fn a_degraded_pass_keeps_both_explicitly_set_halves() {
        let eff = first_pin(&degraded_own(Some("America/Chicago"), Some("10m")));
        assert_eq!(eff.tz.name(), "America/Chicago");
        assert_eq!(eff.jitter.as_deref(), Some("10m"));
        let pin = eff.pin_json(at(2026, 5, 24, 2, 0));
        assert_eq!(pin["timezone"], "America/Chicago");
        assert_eq!(pin["jitter"], "10m");
    }

    #[test]
    fn a_degraded_pass_never_recomputes_for_the_inherited_jitter_half() {
        // Even with an own timezone driving a recompute, the pinned WINDOW is
        // carried through untouched — a degraded pass knows nothing about it.
        let degraded = degraded_own(Some("America/Chicago"), None);
        let pinned = PinnedSlot {
            timezone: Some("UTC"),
            jitter: Some("30m"),
        };
        let (eff, recompute) = resolve_pinned_slot(pinned, &degraded);
        assert!(recompute, "driven by the own-timezone change");
        assert_eq!(
            eff.jitter.as_deref(),
            Some("30m"),
            "the re-pin must carry the pin's own window, not drop it to none"
        );
        // And with no own timezone there is nothing left to recompute at all.
        let (eff, recompute) = resolve_pinned_slot(pinned, &degraded_inherited());
        assert!(!recompute);
        assert_eq!(eff.jitter.as_deref(), Some("30m"));
        assert_eq!(eff.tz, Tz::UTC, "from the pin's own recorded zone");
    }

    #[test]
    fn first_pin_resolved_pins_that_zone() {
        let eff = first_pin(&resolved("America/Chicago"));
        assert_eq!(eff.tz.name(), "America/Chicago");
    }

    #[test]
    fn first_pin_resolved_pins_the_inherited_jitter_window() {
        let eff = first_pin(&resolved_with_jitter("America/Chicago", Some("15m")));
        assert_eq!(eff.tz.name(), "America/Chicago");
        assert_eq!(eff.jitter.as_deref(), Some("15m"));
        assert_eq!(eff.window(), Some(StdDuration::from_secs(900)));
    }

    /// **Byte-identical regression (brief constraint).** A repository that sets ONLY
    /// `scheduleDefaults.timezone` — the pre-jitter world — must behave and pin
    /// EXACTLY as before: no jitter window, no `jitter` key in the pinned status,
    /// and the same `next_fire` instant an un-jittered schedule produced.
    #[test]
    fn timezone_only_repo_default_is_byte_identical_to_the_pre_jitter_behavior() {
        let defaults = tz_defaults("Europe/Berlin");
        assert!(
            defaults.jitter.is_none(),
            "the baseline fixture must set no jitter"
        );
        // Resolution over a fleet of such repos agrees on "no jitter".
        let per_repo: Vec<Option<String>> = vec![defaults.jitter.clone(), defaults.jitter.clone()];
        assert_eq!(
            resolve_schedule_jitter(None, &per_repo),
            ScheduleJitterResolution::Agreed(None)
        );

        let eff = first_pin(&resolved_with_jitter("Europe/Berlin", None));
        // 1. No window reaches `next_fire`, so the slot is the bare cron instant.
        assert_eq!(eff.window(), None);
        let after = at(2026, 5, 24, 3, 0);
        let with_inheritance =
            next_fire("0 2 * * *", eff.window(), "uid-1", after, eff.tz).unwrap();
        let pre_change = next_fire(
            "0 2 * * *",
            None,
            "uid-1",
            after,
            "Europe/Berlin".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(
            with_inheritance, pre_change,
            "a timezone-only repo default must not move the fire time"
        );
        // 2. The pinned status carries a JSON `null` jitter, which a merge patch
        //    treats as "absent" — so the stored object is unchanged from today.
        let pin = eff.pin_json(with_inheritance);
        assert_eq!(pin["at"], with_inheritance.to_rfc3339());
        assert_eq!(pin["timezone"], "Europe/Berlin");
        assert_eq!(pin["jitter"], serde_json::Value::Null);
        // 3. And the pin never goes stale on its own.
        assert!(!pin_needs_recompute(
            PinnedSlot {
                timezone: Some("Europe/Berlin"),
                jitter: None
            },
            eff.tz,
            None
        ));
    }

    /// The fan-out disagreement rule, asserted through BEHAVIOR (the brief): matched
    /// policies' repositories that disagree on `scheduleDefaults.jitter` resolve to
    /// no window, and the resulting fire time is the un-jittered one. The warn is
    /// emitted by `resolve_effective_schedule`; what is load-bearing here is that no
    /// arbitrary window is silently picked.
    #[test]
    fn disagreeing_repo_default_jitter_applies_no_jitter() {
        let disagreeing = [Some("1h".to_string()), Some("10m".to_string())];
        let resolution = resolve_schedule_jitter(None, &disagreeing);
        assert_eq!(
            resolution,
            ScheduleJitterResolution::Disagreed {
                candidates: vec!["10m".to_string(), "1h".to_string()],
            },
            "the disagreement must be reported, not silently swallowed"
        );
        let window = match resolution {
            ScheduleJitterResolution::Agreed(w) => w,
            ScheduleJitterResolution::Disagreed { .. } => None,
        };
        let eff = first_pin(&resolved_with_jitter("UTC", window.as_deref()));
        assert_eq!(eff.window(), None);
        let after = at(2026, 5, 24, 3, 0);
        assert_eq!(
            next_fire("0 2 * * *", eff.window(), "uid-1", after, Tz::UTC).unwrap(),
            next_fire("0 2 * * *", None, "uid-1", after, Tz::UTC).unwrap(),
            "a disagreement must fire exactly where no jitter fires"
        );
        // A window "1h" would have moved it — proving the assertion above has teeth.
        assert_ne!(
            next_fire(
                "0 2 * * *",
                Some(StdDuration::from_secs(3600)),
                "uid-1",
                after,
                Tz::UTC
            )
            .unwrap(),
            next_fire("0 2 * * *", None, "uid-1", after, Tz::UTC).unwrap(),
        );
    }

    /// The projection step the reconciler runs between "GOT the repositories" and
    /// "decided the window": own wins, else the agreed default, else none — and a
    /// repository that sets no `scheduleDefaults` at all reads as "no default" for
    /// BOTH halves rather than disagreeing with one that sets only the other.
    #[test]
    fn inherited_halves_project_each_field_independently() {
        let mixed = [tz_defaults("Europe/Berlin"), jitter_defaults("30m")];
        // Timezone half: Berlin vs a repo with no default (which resolves to UTC)
        // is a genuine disagreement → UTC + the ambiguity condition.
        let (tz, ambiguity) = inherited_timezone(None, &mixed);
        assert_eq!(tz, Tz::UTC);
        assert!(ambiguity.is_some());
        // Jitter half, from the very same slice: `30m` vs no default disagrees too.
        assert!(matches!(
            inherited_jitter(None, &mixed),
            ScheduleJitterResolution::Disagreed { .. }
        ));

        // A fleet agreeing on BOTH fields resolves both cleanly.
        let agreed = [
            ScheduleDefaults {
                timezone: Some("Europe/Berlin".into()),
                jitter: Some("30m".into()),
            },
            ScheduleDefaults {
                timezone: Some("Europe/Berlin".into()),
                jitter: Some("30m".into()),
            },
        ];
        let (tz, ambiguity) = inherited_timezone(None, &agreed);
        assert_eq!(tz.name(), "Europe/Berlin");
        assert!(ambiguity.is_none());
        assert_eq!(
            inherited_jitter(None, &agreed),
            ScheduleJitterResolution::Agreed(Some("30m".into()))
        );

        // Own values short-circuit each half independently — and an own value on
        // one half must NOT suppress inheritance on the other.
        let (tz, ambiguity) = inherited_timezone(Some("America/Chicago"), &agreed);
        assert_eq!(tz.name(), "America/Chicago");
        assert!(
            ambiguity.is_none(),
            "an explicit own timezone is never ambiguous"
        );
        assert_eq!(
            inherited_jitter(None, &agreed),
            ScheduleJitterResolution::Agreed(Some("30m".into())),
            "an own TIMEZONE must not stop the jitter half from inheriting"
        );
        assert_eq!(
            inherited_jitter(Some("5m"), &agreed),
            ScheduleJitterResolution::Agreed(Some("5m".into()))
        );
    }

    #[test]
    fn inherited_halves_with_no_matched_policies_are_utc_and_no_jitter() {
        // A selector matching nothing is a genuine resolution, not a degrade.
        let (tz, ambiguity) = inherited_timezone(None, &[]);
        assert_eq!(tz, Tz::UTC);
        assert!(ambiguity.is_none());
        assert_eq!(
            inherited_jitter(None, &[]),
            ScheduleJitterResolution::Agreed(None)
        );
        // And a single repository setting neither field resolves the same way —
        // this is the shape a pre-jitter cluster is in.
        let bare = [ScheduleDefaults::default()];
        assert_eq!(inherited_timezone(None, &bare).0, Tz::UTC);
        assert_eq!(
            inherited_jitter(None, &bare),
            ScheduleJitterResolution::Agreed(None)
        );
    }

    /// Own `spec.schedule.jitter` beats every repository default, and an ABSENT own
    /// jitter inherits — the two halves of the resolution rule, at the kernel the
    /// reconciler calls.
    #[test]
    fn schedule_jitter_own_wins_else_inherits_else_none() {
        let repo = [Some("1h".to_string())];
        assert_eq!(
            resolve_schedule_jitter(Some("5m"), &repo),
            ScheduleJitterResolution::Agreed(Some("5m".to_string())),
            "own jitter wins over the repo default"
        );
        assert_eq!(
            resolve_schedule_jitter(None, &repo),
            ScheduleJitterResolution::Agreed(Some("1h".to_string())),
            "absent own jitter inherits the repo default"
        );
        assert_eq!(
            resolve_schedule_jitter(None, &[None]),
            ScheduleJitterResolution::Agreed(None),
            "neither set → no jitter"
        );
        assert_eq!(
            resolve_schedule_jitter(None, &[]),
            ScheduleJitterResolution::Agreed(None),
            "no matched policies → nothing to inherit"
        );
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
        // Every fixture is a PRIOR slot's child by default (2023-11-14, far
        // before `REPLACE_SLOT`), so `replace_plan`'s own-slot filter treats it
        // as a legitimate victim. Tests that need this slot's own child use
        // `created_at`.
        s.metadata.creation_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
            k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
        ));
        if deleting {
            s.metadata.deletion_timestamp =
                Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                    k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
                ));
        }
        s
    }

    /// The slot every `replace_plan` test fires. Children from `run_in_phase`
    /// predate it; `created_at` builds one that does not.
    fn replace_slot() -> DateTime<Utc> {
        at(2026, 8, 4, 3, 0)
    }

    /// `run_in_phase` with an explicit creation instant, for the own-slot filter.
    fn created_at(name: &str, phase: Option<SnapshotPhase>, created: DateTime<Utc>) -> Snapshot {
        let mut s = run_in_phase(name, phase, false);
        s.metadata.creation_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
            k8s_openapi::jiff::Timestamp::from_second(created.timestamp()).unwrap(),
        ));
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
        let (conditions, _) =
            schedule_ready_status(&sched, None, Some(ScheduleHold::Unreadable(&blocked)), None);
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

    // --- M4: `concurrencyPolicy: Replace` -----------------------------------

    /// [`run_in_phase`] plus a `RepositorySlotAvailable=False` condition — a
    /// child parked behind its repository's mover-Job concurrency cap.
    fn parked_run(name: &str, phase: Option<SnapshotPhase>) -> Snapshot {
        let mut s = run_in_phase(name, phase, false);
        s.status.as_mut().unwrap().conditions =
            vec![k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
                type_: kopiur_api::consts::REPOSITORY_SLOT_AVAILABLE_CONDITION.into(),
                status: "False".into(),
                reason: "WaitingForSlot".into(),
                message: "queued behind the repository's mover-Job concurrency cap".into(),
                last_transition_time: k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                    k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
                ),
                observed_generation: None,
            }];
        s
    }

    #[test]
    fn replace_plan_truth_table() {
        // Nothing at all → fire normally, delete nothing.
        assert_eq!(replace_plan(&[], replace_slot()), ReplacePlan::Clear);

        // Only terminal children → nothing to replace.
        for terminal in [
            SnapshotPhase::Succeeded,
            SnapshotPhase::Failed,
            SnapshotPhase::Deleting,
            SnapshotPhase::Discovered,
            SnapshotPhase::Unchanged,
        ] {
            assert_eq!(
                replace_plan(
                    &[run_in_phase("a", Some(terminal.clone()), false)],
                    replace_slot()
                ),
                ReplacePlan::Clear,
                "{terminal:?}"
            );
        }

        // The unfinished set matches the concurrency gate's (`None`, Pending,
        // Running) and comes back SORTED, so a fire is deterministic however
        // the store/LIST ordered the rows.
        assert_eq!(
            replace_plan(
                &[
                    run_in_phase("z-running", Some(SnapshotPhase::Running), false),
                    run_in_phase("m-statusless", None, false),
                    run_in_phase("done", Some(SnapshotPhase::Succeeded), false),
                    run_in_phase("a-pending", Some(SnapshotPhase::Pending), false),
                ],
                replace_slot()
            ),
            ReplacePlan::Delete(vec![
                "a-pending".into(),
                "m-statusless".into(),
                "z-running".into()
            ])
        );

        // Rows already terminating are skipped: their finalizer is draining and
        // re-deleting them would only pad the Event.
        assert_eq!(
            replace_plan(
                &[
                    run_in_phase("draining", Some(SnapshotPhase::Running), true),
                    run_in_phase("live", Some(SnapshotPhase::Running), false),
                ],
                replace_slot()
            ),
            ReplacePlan::Delete(vec!["live".into()])
        );
        assert_eq!(
            replace_plan(
                &[run_in_phase("draining", Some(SnapshotPhase::Running), true)],
                replace_slot()
            ),
            ReplacePlan::Clear,
            "a lone terminating child leaves nothing to replace"
        );
    }

    /// **The silent-slot-loss regression.** `Replace` must never cancel the
    /// child THIS slot just minted. Without the `slot` filter, a retried fire
    /// (first attempt minted the child, then errored mid-fan-out or hit a 409 on
    /// the status patch, leaving the pin un-advanced) re-enters with
    /// `disposition == Fire`, sees its own brand-new `Pending` child as an
    /// in-flight run, kills its mover Job and CR — after which
    /// `slot_fire_blocked_by_terminating` skips the re-fire and the status patch
    /// still records the slot as fired. Slot recorded, no backup. Once per retry.
    #[test]
    fn replace_plan_never_cancels_this_slots_own_children() {
        let slot = replace_slot();

        // A child minted AT the slot instant (the retry case) is not a previous
        // run: skipped entirely, so the plan is Clear and the re-fire's
        // server-side apply converges on it.
        assert_eq!(
            replace_plan(
                &[created_at("own", Some(SnapshotPhase::Pending), slot)],
                slot
            ),
            ReplacePlan::Clear,
            "the current slot's own child must never be a victim"
        );
        // Minted a moment AFTER the slot instant (the ordinary case — the fire
        // happens at `now >= slot`): still this slot's own.
        assert_eq!(
            replace_plan(
                &[created_at(
                    "own",
                    Some(SnapshotPhase::Running),
                    slot + chrono::Duration::seconds(30)
                )],
                slot
            ),
            ReplacePlan::Clear
        );
        // One second BEFORE the slot is a previous slot's run — still replaced.
        assert_eq!(
            replace_plan(
                &[created_at(
                    "prior",
                    Some(SnapshotPhase::Running),
                    slot - chrono::Duration::seconds(1)
                )],
                slot
            ),
            ReplacePlan::Delete(vec!["prior".into()])
        );
        // Mixed: only the prior-slot child is cancelled, and the fire proceeds.
        assert_eq!(
            replace_plan(
                &[
                    created_at("own", Some(SnapshotPhase::Pending), slot),
                    created_at(
                        "prior",
                        Some(SnapshotPhase::Running),
                        slot - chrono::Duration::hours(24)
                    ),
                ],
                slot
            ),
            ReplacePlan::Delete(vec!["prior".into()])
        );
        // The own-slot filter runs BEFORE the phase match, so this slot's own
        // child can raise neither hold — a retry must not wedge on its own output.
        let mut own_unknown = created_at("own", Some(SnapshotPhase::Unknown("Odd".into())), slot);
        own_unknown.metadata.name = Some("own".into());
        assert_eq!(replace_plan(&[own_unknown], slot), ReplacePlan::Clear);
        let mut own_parked = parked_run("own", Some(SnapshotPhase::Pending));
        own_parked.metadata.creation_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::from_second(slot.timestamp()).unwrap(),
            ));
        assert_eq!(replace_plan(&[own_parked], slot), ReplacePlan::Clear);

        // Fail closed on an UNDATABLE row: it cannot be proven to belong to a
        // previous slot, and wrongly deleting a run is far worse than wrongly
        // degrading one slot to `Allow`.
        let mut undated = run_in_phase("undated", Some(SnapshotPhase::Running), false);
        undated.metadata.creation_timestamp = None;
        assert_eq!(
            replace_plan(&[undated], slot),
            ReplacePlan::Clear,
            "a child with no creationTimestamp must never be cancelled"
        );
    }

    #[test]
    fn replace_plan_fails_closed_on_an_unreadable_child() {
        // An unreadable phase wins over known-unfinished siblings: Replace must
        // never DELETE what it cannot classify (the run may be live, written by
        // a newer operator), and it must not mint beside it either — so the
        // whole slot is handed to the same `ScheduleRunnable` gate `Forbid` uses.
        assert_eq!(
            replace_plan(
                &[
                    run_in_phase("running-sibling", Some(SnapshotPhase::Running), false),
                    run_in_phase(
                        "nightly-20260807",
                        Some(SnapshotPhase::Unknown("Quiescing".into())),
                        false,
                    ),
                    run_in_phase("pending-sibling", Some(SnapshotPhase::Pending), false),
                ],
                replace_slot()
            ),
            ReplacePlan::BlockedUnreadable(UnreadableRun {
                snapshot: "nightly-20260807".into(),
                phase: "Quiescing".into(),
            }),
            "an Unknown phase must beat known-unfinished children (fail closed)"
        );

        // Fail-closed outranks the parked hold too — the unreadable row is the
        // one that needs a human, and it names the same object `classify_active_runs`
        // names, so the two gates never disagree.
        assert_eq!(
            replace_plan(
                &[
                    parked_run("parked", Some(SnapshotPhase::Pending)),
                    run_in_phase(
                        "skewed",
                        Some(SnapshotPhase::Unknown("Quiescing".into())),
                        false
                    ),
                ],
                replace_slot()
            ),
            ReplacePlan::BlockedUnreadable(UnreadableRun {
                snapshot: "skewed".into(),
                phase: "Quiescing".into(),
            })
        );

        // A TERMINATING unreadable row is skipped like any other terminating
        // row — it is not a blocker, so the live sibling is replaced normally.
        assert_eq!(
            replace_plan(
                &[
                    run_in_phase(
                        "skewed",
                        Some(SnapshotPhase::Unknown("Quiescing".into())),
                        true
                    ),
                    run_in_phase("live", Some(SnapshotPhase::Running), false),
                ],
                replace_slot()
            ),
            ReplacePlan::Delete(vec!["live".into()])
        );
    }

    #[test]
    fn replace_plan_holds_while_a_child_is_parked_behind_the_repository_cap() {
        // A parked child is QUEUED, not running. Deleting it frees no slot and
        // the replacement parks right behind it — a delete-mint-park livelock
        // that burns a CR per slot forever.
        assert_eq!(
            replace_plan(
                &[parked_run("queued", Some(SnapshotPhase::Pending))],
                replace_slot()
            ),
            ReplacePlan::HeldByParkedRun("queued".into())
        );

        // The audit-critical case: a parked child alongside a genuinely RUNNING
        // sibling still holds — nothing is deleted. Minting beside the queue is
        // exactly the pileup the repository cap exists to prevent.
        let plan = replace_plan(
            &[
                run_in_phase("running-sibling", Some(SnapshotPhase::Running), false),
                parked_run("queued", Some(SnapshotPhase::Pending)),
            ],
            replace_slot(),
        );
        assert_eq!(plan, ReplacePlan::HeldByParkedRun("queued".into()));
        assert!(
            !matches!(plan, ReplacePlan::Delete(_)),
            "a parked child must delete NOTHING, not even a running sibling"
        );

        // `True` (the healed state) is not parked, so the pool draining lets the
        // very next reconcile replace normally — the hold self-clears.
        let mut healed = parked_run("healed", Some(SnapshotPhase::Running));
        healed.status.as_mut().unwrap().conditions[0].status = "True".into();
        assert_eq!(
            replace_plan(&[healed], replace_slot()),
            ReplacePlan::Delete(vec!["healed".into()])
        );

        // A stale `False` on a FINISHED child must never hold the schedule: the
        // parked check is scoped to the unfinished, non-terminating set.
        assert_eq!(
            replace_plan(
                &[parked_run("stale", Some(SnapshotPhase::Succeeded))],
                replace_slot()
            ),
            ReplacePlan::Clear
        );
    }

    /// A mock client that answers per `(method, path)` and logs every request,
    /// so the replacement's ORDER (verify → Job delete → stamp → CR delete) is
    /// asserted rather than assumed.
    fn scripted_client<F>(log: Arc<std::sync::Mutex<Vec<String>>>, respond: F) -> kube::Client
    where
        F: Fn(&http::Method, &str) -> (StatusCode, serde_json::Value) + Send + Sync + 'static,
    {
        let respond = Arc::new(respond);
        let svc = tower::service_fn(move |req: Request<Body>| {
            let log = log.clone();
            let respond = respond.clone();
            async move {
                let (method, path) = (req.method().clone(), req.uri().path().to_string());
                log.lock().unwrap().push(format!("{method} {path}"));
                let (status, body) = respond(&method, &path);
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
            }
        });
        kube::Client::new(svc, "default")
    }

    fn ok_status() -> serde_json::Value {
        serde_json::json!({
            "kind": "Status", "apiVersion": "v1", "status": "Success", "code": 200,
        })
    }

    fn live_snapshot_body(ns: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": crate::consts::API_VERSION,
            "kind": "Snapshot",
            "metadata": { "name": name, "namespace": ns, "uid": format!("uid-{name}") },
            "spec": {},
        })
    }

    fn schedule_fixture(ns: &str, name: &str, policy: ConcurrencyPolicy) -> SnapshotSchedule {
        let mut s = SnapshotSchedule::new(
            name,
            kopiur_api::SnapshotScheduleSpec {
                schedule: schedule_spec("0 3 * * *", false, policy, None),
                policy_ref: Some(PolicyRef {
                    name: "pg".into(),
                    namespace: None,
                }),
                policy_selector: None,
                failed_jobs_history_limit: None,
                deletion: None,
            },
        );
        s.metadata.namespace = Some(ns.into());
        s.metadata.uid = Some(format!("uid-{name}"));
        s
    }

    /// **The IMPORTANT-1 regression, through the real reconcile.** A schedule that
    /// sets `spec.schedule.jitter` but no `timezone` on its FIRST reconcile, while
    /// its repository is unreadable (the routine GitOps bundle-apply ordering). The
    /// self-heal pin must still carry the window the user explicitly asked for —
    /// pinning it un-jittered would be permanent for that pin, because
    /// `pin_needs_recompute` never revisits an absent recorded window.
    #[tokio::test]
    async fn a_degraded_first_pin_records_the_schedules_own_jitter() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = body_recording_client(log.clone(), |_method, path| {
            if path.contains("/snapshotpolicies/") {
                return (StatusCode::OK, policy_body("apps", "pg"));
            }
            if path.contains("/repositories/") {
                // The referent is not there yet (or not readable yet).
                return (StatusCode::INTERNAL_SERVER_ERROR, ok_status());
            }
            if path.contains("/snapshotschedules/") {
                return (StatusCode::OK, schedule_body("apps", "nightly"));
            }
            (StatusCode::OK, ok_status())
        });
        let ctx = Context::test_context(client);
        prime_snapshot_store(&ctx, vec![], true);

        // No status at all (first reconcile), own jitter, inherited timezone.
        let mut schedule = schedule_fixture("apps", "nightly", ConcurrencyPolicy::Forbid);
        schedule.spec.schedule.jitter = Some("10m".into());
        reconcile_inner(&schedule, &ctx)
            .await
            .expect("a degraded first pin must not fail the reconcile");

        let requests = log.lock().unwrap().clone();
        let pin = requests
            .iter()
            .find(|(line, _)| line.contains("/snapshotschedules/nightly/status"))
            .and_then(|(_, body)| body.pointer("/status/nextSchedule").cloned())
            .expect("a first reconcile must pin nextSchedule");
        assert_eq!(
            pin["jitter"], "10m",
            "the own window needed no lookup — a degraded pass must still pin it, or \
             this schedule fires un-jittered forever"
        );
        assert_eq!(
            pin["timezone"], "UTC",
            "the INHERITED half is the one that self-heals"
        );
    }

    /// A `SnapshotSchedule` with `nextSchedule` already pinned to a PAST slot in
    /// a fixed zone, so `reconcile_inner` enters the pinned-slot branch with
    /// `disposition == Fire`.
    ///
    /// It sets an explicit `schedule.timezone` but no `schedule.jitter`, so
    /// `resolve_effective_schedule` still consults the target policy's repository
    /// for the jitter half — and, against these fixtures' scripted clients (which
    /// answer `/repositories/` with a bare `Status`), that read fails and the pass
    /// is `Degraded`. That is deliberately harmless here and is exactly the
    /// invariant under test elsewhere: `Degraded` keeps the pin's own recorded
    /// timezone (UTC) and window (none) in effect, so the fire path below behaves
    /// identically to a clean resolution.
    fn pinned_schedule(ns: &str, name: &str, slot: DateTime<Utc>) -> SnapshotSchedule {
        let mut s = schedule_fixture(ns, name, ConcurrencyPolicy::Replace);
        s.spec.schedule.timezone = Some("UTC".into());
        s.metadata.generation = Some(3);
        s.status = Some(kopiur_api::SnapshotScheduleStatus {
            next_schedule: Some(kopiur_api::snapshot_schedule::ScheduleRef {
                at: Some(slot.to_rfc3339()),
                timezone: Some("UTC".into()),
                jitter: None,
                snapshot_ref: None,
            }),
            ..Default::default()
        });
        s
    }

    /// A minimal `SnapshotPolicy` body the fire path can read (no pvcSelector ⇒
    /// no PVC listing, one unpinned child).
    fn policy_body(ns: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": crate::consts::API_VERSION,
            "kind": "SnapshotPolicy",
            "metadata": { "name": name, "namespace": ns, "uid": "uid-pg" },
            "spec": { "repository": { "name": "repo" } },
        })
    }

    /// **The deliverable-4 wiring test, and the CRITICAL-1 regression guard.**
    ///
    /// Drives the real `reconcile_inner` for a `Replace` schedule whose pinned
    /// slot is due and which has one `Running` child from a PREVIOUS slot. It
    /// asserts the whole wired path in one pass: the old run's mover Job is
    /// deleted, its CR is stamped `pruned-by: replaced-run` and deleted, a
    /// `ReplacedActiveRun` event is published, and the new slot's child is minted
    /// — all before the schedule's status patch.
    ///
    /// The second half is the regression: the freshly-minted child is fed back in
    /// and the SAME reconcile is retried (the crashed-before-status-patch shape).
    /// The retry must cancel NOTHING — pre-fix it would delete its own output and
    /// the slot would be recorded as fired with no backup behind it.
    #[tokio::test]
    async fn reconcile_replace_cancels_the_prior_run_mints_the_new_one_and_never_eats_its_own() {
        let slot = at(2026, 8, 4, 3, 0);
        let ns = "apps";
        let new_child = scheduled_backup_name("nightly", slot);

        // One request log per pass, plus a switchable children population.
        let run_pass = |children: Vec<Snapshot>| async move {
            let log = Arc::new(std::sync::Mutex::new(Vec::new()));
            let client = scripted_client(log.clone(), move |method, path| {
                if path.contains("/snapshotpolicies/") {
                    return (StatusCode::OK, policy_body("apps", "pg"));
                }
                if path.contains("/snapshotschedules/") {
                    return (
                        StatusCode::OK,
                        serde_json::json!({
                            "apiVersion": crate::consts::API_VERSION,
                            "kind": "SnapshotSchedule",
                            "metadata": { "name": "nightly", "namespace": "apps", "uid": "uid-nightly" },
                            "spec": { "schedule": { "cron": "0 3 * * *" }, "policyRef": { "name": "pg" } },
                        }),
                    );
                }
                if method != http::Method::DELETE && path.contains("/snapshots/") {
                    return (StatusCode::OK, live_snapshot_body("apps", "prior-run"));
                }
                (StatusCode::OK, ok_status())
            });
            let ctx = Context::test_context(client);
            // A synced store serves the children AND `slot_twin` (so the new
            // child's absence is read from the cache, no extra GET).
            prime_snapshot_store(&ctx, children, true);
            let schedule = pinned_schedule("apps", "nightly", slot);
            let action = reconcile_inner(&schedule, &ctx).await;
            let requests = log.lock().unwrap().clone();
            (action, requests)
        };

        // --- Pass 1: a PRIOR slot's Running child is replaced -----------------
        let mut prior = created_at(
            "prior-run",
            Some(SnapshotPhase::Running),
            slot - chrono::Duration::hours(24),
        );
        prior.metadata.namespace = Some(ns.into());
        prior.metadata.labels = Some(BTreeMap::from([(
            crate::consts::SCHEDULE_LABEL.to_string(),
            "nightly".to_string(),
        )]));
        prior.spec.on_schedule_delete = Some(ScheduleDeletePolicy::Retain);

        let (action, requests) = run_pass(vec![prior.clone()]).await;
        assert!(action.is_ok(), "reconcile failed: {action:?}");

        let joined = requests.join("\n");
        assert!(
            requests
                .iter()
                .any(|r| r.starts_with("DELETE ") && r.contains("/jobs/prior-run")),
            "the prior run's mover Job must be deleted:\n{joined}"
        );
        let stamp = requests
            .iter()
            .position(|r| r.starts_with("PATCH ") && r.contains("/snapshots/prior-run"))
            .expect("the prior run must be stamped pruned-by");
        let cr_delete = requests
            .iter()
            .position(|r| r.starts_with("DELETE ") && r.contains("/snapshots/prior-run"))
            .expect("the prior run's CR must be deleted");
        assert!(stamp < cr_delete, "stamp before delete:\n{joined}");
        assert!(
            requests.iter().any(|r| r.contains("/events")),
            "a ReplacedActiveRun event must be published:\n{joined}"
        );
        // The new slot's child is minted in the SAME pass...
        let mint = requests
            .iter()
            .position(|r| r.starts_with("PATCH ") && r.contains(&format!("/snapshots/{new_child}")))
            .unwrap_or_else(|| panic!("the new slot child must be minted:\n{joined}"));
        // ...after the cancellation, and before the schedule's status patch.
        assert!(cr_delete < mint, "cancel before mint:\n{joined}");
        let status = requests
            .iter()
            .position(|r| r.contains("/snapshotschedules/nightly/status"))
            .expect("the schedule status must be patched");
        assert!(mint < status, "mint before the status patch:\n{joined}");

        // --- Pass 2: the RETRY, with this slot's own child present ------------
        // Exactly the crashed-before-the-status-patch state: the pin is still on
        // `slot`, so the retry re-enters at `Fire` and sees its own minted child.
        let mut own = created_at(&new_child, Some(SnapshotPhase::Pending), slot);
        own.metadata.namespace = Some(ns.into());
        own.metadata.labels = Some(BTreeMap::from([(
            crate::consts::SCHEDULE_LABEL.to_string(),
            "nightly".to_string(),
        )]));
        own.spec.on_schedule_delete = Some(ScheduleDeletePolicy::Retain);

        let (action, requests) = run_pass(vec![own]).await;
        assert!(action.is_ok(), "retry reconcile failed: {action:?}");
        let joined = requests.join("\n");
        assert!(
            !requests.iter().any(|r| r.starts_with("DELETE ")),
            "a retry must cancel NOTHING — it would be deleting its own just-minted \
             child, and the slot would be recorded as fired with no backup:\n{joined}"
        );
        assert!(
            !requests.iter().any(|r| r.contains("/events")),
            "no run was replaced, so no ReplacedActiveRun event:\n{joined}"
        );
        assert!(
            requests
                .iter()
                .any(|r| r.starts_with("PATCH ") && r.contains(&format!("/snapshots/{new_child}"))),
            "the retry re-applies the same child idempotently:\n{joined}"
        );
    }

    /// The executor's contract, in order: live-verify, then STOP THE MOVER, then
    /// stamp `pruned-by: replaced-run`, then delete the CR. Job-before-CR is the
    /// load-bearing half — the finalizer releases immediately while
    /// `status.snapshot` is absent (the normal mid-run state), so a CR deleted
    /// first would be gone while its mover kept uploading.
    #[tokio::test]
    async fn replace_active_runs_stops_the_mover_then_stamps_and_deletes_the_cr() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = scripted_client(log.clone(), |method, path| {
            // The live-verify GET and the `pruned-by` stamp PATCH both need a
            // real `Snapshot` back; every delete (Job, ConfigMap, CR) answers
            // with a bare Status.
            if method != http::Method::DELETE && path.contains("/snapshots/") {
                (StatusCode::OK, live_snapshot_body("apps", "nightly-run"))
            } else {
                (StatusCode::OK, ok_status())
            }
        });
        let ctx = Context::test_context(client);
        let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), "apps");
        let schedule = schedule_fixture("apps", "nightly", ConcurrencyPolicy::Replace);

        let deleted =
            replace_active_runs(&ctx, &schedule, "apps", &api, &["nightly-run".to_string()])
                .await
                .expect("replacement succeeds");
        assert_eq!(deleted, vec!["nightly-run".to_string()]);

        let requests = log.lock().unwrap().clone();
        // 1. live verify (GET the Snapshot)
        assert!(
            requests[0].starts_with("GET ") && requests[0].ends_with("/snapshots/nightly-run"),
            "the live verify must come first: {requests:?}"
        );
        // 2. the mover Job — SAME name as the Snapshot — dies before the CR.
        let job_delete = requests
            .iter()
            .position(|r| r.starts_with("DELETE ") && r.contains("/jobs/nightly-run"))
            .expect("the mover Job must be deleted");
        // 3. the stamp, 4. the CR delete.
        let stamp = requests
            .iter()
            .position(|r| r.starts_with("PATCH ") && r.contains("/snapshots/nightly-run"))
            .expect("the pruned-by stamp must be applied");
        let cr_delete = requests
            .iter()
            .position(|r| r.starts_with("DELETE ") && r.contains("/snapshots/nightly-run"))
            .expect("the Snapshot CR must be deleted");
        assert!(
            job_delete < stamp && stamp < cr_delete,
            "order must be Job delete → stamp → CR delete, got {requests:?}"
        );
    }

    /// #382 C2: a victim the LIVE read says is gone (or terminating) is skipped
    /// entirely — no Job delete, no stamp, no CR delete, and crucially no Event,
    /// so a stale reflector snapshot cannot re-announce an already-replaced run.
    #[tokio::test]
    async fn replace_active_runs_skips_a_victim_that_is_already_gone() {
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
        let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), "apps");
        let schedule = schedule_fixture("apps", "nightly", ConcurrencyPolicy::Replace);

        let deleted = replace_active_runs(&ctx, &schedule, "apps", &api, &["vanished".to_string()])
            .await
            .expect("a vanished victim is not an error");
        assert!(
            deleted.is_empty(),
            "a skipped victim must not be reported as replaced (no Event names it)"
        );
        let requests = log.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            1,
            "exactly one verify GET and nothing else, got {requests:?}"
        );
        assert!(requests[0].starts_with("GET "), "{requests:?}");
    }

    /// The live verify re-checks PHASE, not just existence. A `Running` victim
    /// can commit its kopia snapshot and flip to `Succeeded` between the
    /// (possibly store-derived) selection and this delete; cancelling it then
    /// deletes the CR of a COMPLETED backup and leaves its snapshot unreferenced.
    #[test]
    fn still_replaceable_requires_the_victim_to_be_unfinished() {
        // Gone / terminating — the pre-existing #382 C2 rules.
        assert!(!still_replaceable(None));
        assert!(!still_replaceable(Some(&run_in_phase(
            "draining",
            Some(SnapshotPhase::Running),
            true
        ))));
        // Still in flight ⇒ cancellable.
        for unfinished in [
            None,
            Some(SnapshotPhase::Pending),
            Some(SnapshotPhase::Running),
        ] {
            assert!(
                still_replaceable(Some(&run_in_phase("live", unfinished.clone(), false))),
                "{unfinished:?}"
            );
        }
        // Finished after the selection ⇒ NOT cancellable. `Succeeded` is the
        // data-loss case this guard exists for.
        for finished in [
            SnapshotPhase::Succeeded,
            SnapshotPhase::Failed,
            SnapshotPhase::Deleting,
            SnapshotPhase::Discovered,
            SnapshotPhase::Unchanged,
            SnapshotPhase::Unknown("Quiescing".into()),
        ] {
            assert!(
                !still_replaceable(Some(&run_in_phase("done", Some(finished.clone()), false))),
                "{finished:?} must not be cancelled"
            );
        }
    }

    /// End-to-end through the executor: a victim that finished after selection
    /// costs exactly one GET and is neither cancelled nor announced.
    #[tokio::test]
    async fn replace_active_runs_skips_a_victim_that_finished_after_selection() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut succeeded = live_snapshot_body("apps", "nightly-run");
        succeeded["status"] = serde_json::json!({ "phase": "Succeeded" });
        let client = scripted_client(log.clone(), move |_m, _p| {
            (StatusCode::OK, succeeded.clone())
        });
        let ctx = Context::test_context(client);
        let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), "apps");
        let schedule = schedule_fixture("apps", "nightly", ConcurrencyPolicy::Replace);

        let deleted =
            replace_active_runs(&ctx, &schedule, "apps", &api, &["nightly-run".to_string()])
                .await
                .expect("a finished victim is not an error");
        assert!(
            deleted.is_empty(),
            "a backup that completed after selection must never be cancelled"
        );
        let requests = log.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            1,
            "one verify GET, then nothing — no Job delete, no stamp, no CR delete: {requests:?}"
        );
        assert!(requests[0].starts_with("GET "), "{requests:?}");
    }

    /// The plan drives the disposition, and only for `Replace`: `Forbid` and
    /// `Allow` must be byte-identical to their pre-M4 behavior (no plan is even
    /// computed for them, so no victim can ever be selected).
    #[test]
    fn only_replace_computes_a_plan_and_forbid_allow_are_unchanged() {
        let slot = at(2026, 8, 4, 3, 0);
        let due = at(2026, 8, 4, 3, 5);
        let in_flight = [run_in_phase("run", Some(SnapshotPhase::Running), false)];
        let runs = classify_active_runs(&in_flight);
        assert!(runs.active);

        // Forbid: an active run still simply waits — the pinned slot is the
        // single catch-up, no deletes, exactly as before.
        assert_eq!(
            slot_disposition(
                &schedule_spec("0 3 * * *", false, ConcurrencyPolicy::Forbid, None),
                slot,
                due,
                runs.active
            ),
            SlotDisposition::Wait
        );
        // Allow: the declared-overlap contract — fires alongside, no deletes.
        assert_eq!(
            slot_disposition(
                &schedule_spec("0 3 * * *", false, ConcurrencyPolicy::Allow, None),
                slot,
                due,
                runs.active
            ),
            SlotDisposition::Fire
        );
        // Replace: also Fire from `slot_disposition`'s point of view (that is
        // what `concurrency_allows` returning true means) — the replacement is
        // then what `replace_plan` decides on the SAME children.
        assert_eq!(
            slot_disposition(
                &schedule_spec("0 3 * * *", false, ConcurrencyPolicy::Replace, None),
                slot,
                due,
                runs.active
            ),
            SlotDisposition::Fire
        );
        assert_eq!(
            replace_plan(&in_flight, replace_slot()),
            ReplacePlan::Delete(vec!["run".into()])
        );
    }

    /// The wiring invariant the reconciler leans on: when `replace_plan` says
    /// `BlockedUnreadable`, `classify_active_runs` must independently produce
    /// the SAME `UnreadableRun`. The Fire→Wait downgrade re-uses
    /// `runs.unreadable` (not the plan's copy) to feed the `ScheduleRunnable`
    /// gate, so if the two ever picked different children — or one found a
    /// blocker the other didn't — `Replace` would downgrade to Wait with an
    /// EMPTY blocker and hang at the 1s requeue floor with nothing explaining why.
    #[test]
    fn replace_and_classify_agree_on_the_unreadable_blocker() {
        let children = [
            run_in_phase("finished", Some(SnapshotPhase::Succeeded), false),
            run_in_phase("also-running", Some(SnapshotPhase::Running), false),
            run_in_phase(
                "skewed-a",
                Some(SnapshotPhase::Unknown("Quiescing".into())),
                false,
            ),
            run_in_phase(
                "skewed-b",
                Some(SnapshotPhase::Unknown("Draining".into())),
                false,
            ),
        ];
        let runs = classify_active_runs(&children);
        let blocker = runs.unreadable.expect("the gate must name a blocker");
        assert_eq!(
            replace_plan(&children, replace_slot()),
            ReplacePlan::BlockedUnreadable(blocker.clone()),
            "both kernels must name the FIRST unreadable child"
        );
        assert_eq!(blocker.snapshot, "skewed-a");

        // And the converse: no unreadable child ⇒ neither reports one, so the
        // downgrade never fires and the plan is free to delete.
        let readable = [run_in_phase("run", Some(SnapshotPhase::Running), false)];
        assert_eq!(classify_active_runs(&readable).unreadable, None);
        assert!(matches!(
            replace_plan(&readable, replace_slot()),
            ReplacePlan::Delete(_)
        ));
    }

    // --- scheduleDefaults.jitter inheritance, through `reconcile_inner` --------

    /// A mock client that answers per `(method, path)` AND records every request
    /// BODY, so the pinned `status.nextSchedule.jitter` a reconcile writes can be
    /// asserted rather than inferred from the request path alone.
    fn body_recording_client<F>(
        log: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
        respond: F,
    ) -> kube::Client
    where
        F: Fn(&http::Method, &str) -> (StatusCode, serde_json::Value) + Send + Sync + 'static,
    {
        let respond = Arc::new(respond);
        let svc = tower::service_fn(move |req: Request<Body>| {
            let log = log.clone();
            let respond = respond.clone();
            async move {
                let (method, path) = (req.method().clone(), req.uri().path().to_string());
                let (status, response) = respond(&method, &path);
                let bytes = http_body_util::BodyExt::collect(req.into_body())
                    .await
                    .map(|c| c.to_bytes())
                    .unwrap_or_default();
                let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                log.lock().unwrap().push((format!("{method} {path}"), body));
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&response).unwrap()))
                        .unwrap(),
                )
            }
        });
        kube::Client::new(svc, "default")
    }

    /// The `SnapshotSchedule` body the apiserver echoes back from a status PATCH
    /// (kube deserializes the response, so a bare `Status` will not do).
    fn schedule_body(ns: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": crate::consts::API_VERSION,
            "kind": "SnapshotSchedule",
            "metadata": { "name": name, "namespace": ns, "uid": format!("uid-{name}") },
            "spec": { "schedule": { "cron": "0 3 * * *" }, "policyRef": { "name": "pg" } },
        })
    }

    /// A `Repository` the schedule's target policy points at, carrying
    /// `scheduleDefaults` verbatim.
    fn repository_body(
        ns: &str,
        name: &str,
        schedule_defaults: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": crate::consts::API_VERSION,
            "kind": "Repository",
            "metadata": { "name": name, "namespace": ns, "uid": "uid-repo" },
            "spec": {
                "backend": { "filesystem": { "path": "/repo" } },
                "encryption": { "passwordSecretRef": { "name": "pw" } },
                "scheduleDefaults": schedule_defaults,
            },
        })
    }

    /// Drive `reconcile_inner` for a schedule whose `nextSchedule` is pinned in the
    /// FUTURE (so the not-due `Wait` arm is taken and the only status write, if any,
    /// is the pin recompute) against a repository advertising `schedule_defaults`.
    /// Returns the `nextSchedule` object of the status patch, if one was written.
    async fn reconcile_with_repo_defaults(
        schedule: SnapshotSchedule,
        schedule_defaults: serde_json::Value,
    ) -> Option<serde_json::Value> {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = body_recording_client(log.clone(), move |_method, path| {
            if path.contains("/snapshotpolicies/") {
                return (StatusCode::OK, policy_body("apps", "pg"));
            }
            if path.contains("/repositories/") {
                return (
                    StatusCode::OK,
                    repository_body("apps", "repo", schedule_defaults.clone()),
                );
            }
            if path.contains("/snapshotschedules/") {
                return (StatusCode::OK, schedule_body("apps", "nightly"));
            }
            (StatusCode::OK, ok_status())
        });
        let ctx = Context::test_context(client);
        // An empty, synced Snapshot store: no children, so no active run.
        prime_snapshot_store(&ctx, vec![], true);
        reconcile_inner(&schedule, &ctx)
            .await
            .expect("reconcile must succeed");
        let requests = log.lock().unwrap().clone();
        requests
            .iter()
            .find(|(line, _)| line.contains("/snapshotschedules/nightly/status"))
            .and_then(|(_, body)| body.pointer("/status/nextSchedule").cloned())
    }

    /// A schedule pinned to a FUTURE slot, inheriting BOTH timing inputs (no own
    /// `timezone`/`jitter`), with `pinned_jitter` recorded on the pin.
    fn inheriting_pinned_schedule(pinned_jitter: Option<&str>) -> SnapshotSchedule {
        let mut s = schedule_fixture("apps", "nightly", ConcurrencyPolicy::Forbid);
        s.metadata.generation = Some(3);
        s.status = Some(kopiur_api::SnapshotScheduleStatus {
            next_schedule: Some(kopiur_api::snapshot_schedule::ScheduleRef {
                // Far enough out that the slot is never due while the test runs.
                at: Some((Utc::now() + chrono::Duration::days(30)).to_rfc3339()),
                timezone: Some("UTC".into()),
                jitter: pinned_jitter.map(str::to_string),
                snapshot_ref: None,
            }),
            ..Default::default()
        });
        s
    }

    /// **The pin-invalidation deliverable, end to end.** A schedule pinned with a
    /// `10m` window whose repository now advertises `scheduleDefaults.jitter: 30m`
    /// must recompute its pinned slot THIS reconcile and re-pin the NEW window —
    /// not take the change an arbitrary slot later.
    #[tokio::test]
    async fn a_changed_repo_default_jitter_re_pins_next_schedule_with_the_new_window() {
        let pin = reconcile_with_repo_defaults(
            inheriting_pinned_schedule(Some("10m")),
            serde_json::json!({ "jitter": "30m" }),
        )
        .await
        .expect("the stale pin must be recomputed, which writes status");
        assert_eq!(
            pin["jitter"], "30m",
            "the re-pin must record the NEWLY inherited window"
        );
        assert_eq!(pin["timezone"], "UTC");
        assert!(
            pin["at"].as_str().is_some(),
            "a recomputed slot must be pinned"
        );
    }

    /// **The upgrade-churn rule, end to end.** A pre-upgrade pin records no jitter.
    /// An inherited window must NOT retroactively invalidate it — otherwise every
    /// pinned schedule in the cluster recomputes on the operator upgrade. The
    /// not-due arm writes no `nextSchedule` at all.
    #[tokio::test]
    async fn an_absent_pinned_jitter_does_not_churn_on_upgrade() {
        let pin = reconcile_with_repo_defaults(
            inheriting_pinned_schedule(None),
            serde_json::json!({ "jitter": "30m" }),
        )
        .await;
        assert!(
            pin.is_none(),
            "a legacy pin with no recorded window must not be recomputed, got {pin:?}"
        );
    }

    /// **Byte-identical regression, end to end.** A repository setting ONLY
    /// `scheduleDefaults.timezone` — the pre-jitter world — must leave an
    /// established pin completely untouched.
    #[tokio::test]
    async fn a_timezone_only_repo_default_leaves_an_established_pin_untouched() {
        let pin = reconcile_with_repo_defaults(
            inheriting_pinned_schedule(None),
            serde_json::json!({ "timezone": "UTC" }),
        )
        .await;
        assert!(
            pin.is_none(),
            "a timezone-only default must not rewrite the pin, got {pin:?}"
        );
    }

    /// The steady state after inheritance has landed: pinned window == inherited
    /// window ⇒ no recompute, no status churn on every reconcile.
    #[tokio::test]
    async fn an_unchanged_inherited_jitter_window_is_patch_free() {
        let pin = reconcile_with_repo_defaults(
            inheriting_pinned_schedule(Some("30m")),
            serde_json::json!({ "jitter": "30m" }),
        )
        .await;
        assert!(
            pin.is_none(),
            "the steady state must not re-pin every reconcile, got {pin:?}"
        );
    }

    /// A repository GET failure (`Degraded`) must keep BOTH pinned timing inputs —
    /// the transient-blip invariant, driven through the real reconcile rather than
    /// only the pure kernel.
    #[tokio::test]
    async fn a_repository_read_failure_leaves_the_pin_untouched() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = body_recording_client(log.clone(), |_method, path| {
            if path.contains("/snapshotpolicies/") {
                return (StatusCode::OK, policy_body("apps", "pg"));
            }
            if path.contains("/repositories/") {
                // The apiserver blip: the referent cannot be read this pass.
                return (StatusCode::INTERNAL_SERVER_ERROR, ok_status());
            }
            if path.contains("/snapshotschedules/") {
                return (StatusCode::OK, schedule_body("apps", "nightly"));
            }
            (StatusCode::OK, ok_status())
        });
        let ctx = Context::test_context(client);
        prime_snapshot_store(&ctx, vec![], true);
        // Pinned with a 10m window; a Degraded pass must not touch it even though
        // "resolution failed" would otherwise look like "no jitter".
        let schedule = inheriting_pinned_schedule(Some("10m"));
        reconcile_inner(&schedule, &ctx)
            .await
            .expect("a degraded pass must not fail the reconcile");
        let requests = log.lock().unwrap().clone();
        let repinned = requests
            .iter()
            .find(|(line, _)| line.contains("/snapshotschedules/nightly/status"))
            .and_then(|(_, body)| body.pointer("/status/nextSchedule").cloned());
        assert!(
            repinned.is_none(),
            "a transient referent failure must never move an established pin, got {repinned:?}"
        );
    }
}
