//! On-demand ("run it now") plumbing shared by the two replication kinds
//! (`RepositoryReplication`, `SnapshotReplication`) — issue #380.
//!
//! The shape mirrors `Maintenance`'s manual run minus `run-mode` (a replication
//! has exactly one kind of run): annotate the CR with
//! [`crate::consts::RUN_REQUESTED_ANNOTATION`] (an RFC3339 timestamp) and the
//! reconciler drives ONE run through the **existing** per-slot spawn path,
//! answering in `status.manualRun`. Nothing about the mover changes, and no
//! gate is bypassed: a manual run is a cron run with a different Job name and
//! the request instant as its slot.
//!
//! This module also owns the single place either controller turns an observed
//! terminal mover Job into `kopiur_replication_runs_total`. That is here, and
//! not in the reconcile's Job-outcome arms, because those arms are *not*
//! once-per-run:
//!
//! * the cron **success** arm is barely reachable at all — the mover stamps
//!   `status.lastReplicated` before its Job goes terminal, and `due_slot`
//!   anchors on exactly that field, so by the time the Job is observable as
//!   succeeded the slot is usually no longer due and the reconcile takes the
//!   idle arm instead;
//! * the cron **failure** arm is reached over and over (every `REQUEUE_FAILED`)
//!   for one failed run, because a failure does not advance `lastReplicated`
//!   and the slot stays due until the Job TTL-reaps.
//!
//! Counting there would therefore under-count successes to ~zero and
//! over-count failures without bound. Instead [`observe_and_count_runs`] reads
//! the CR's Jobs once per reconcile and counts each terminal Job exactly once,
//! stamping [`crate::consts::RUN_COUNTED_ANNOTATION`] on the Job as the durable
//! "already counted" marker. The marker rides the Job, so it self-cleans with
//! the Job's TTL and survives an operator restart. The one bounded gap is a Job
//! that TTL-reaps before any reconcile sees it terminal; the replication TTL
//! (1h) is comfortably longer than the requeue cap (30m), so every reconcile
//! loop gets at least one look.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::Api;
use kube::api::{ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;

use kopiur_api::common::{PhaseLabel, ReplicationManualRunPhase, ReplicationManualRunStatus};

use crate::consts::{
    COMPONENT_LABEL, REPLICATION_COMPONENT, REPLICATION_INSTANCE_LABEL, RUN_COUNTED_ANNOTATION,
    RUN_REQUESTED_ANNOTATION, RUN_TRIGGER_ANNOTATION, SNAPSHOT_REPLICATION_COMPONENT,
    SNAPSHOT_REPLICATION_INSTANCE_LABEL,
};
use crate::context::Context;
use crate::error::{Error, Result};
use crate::metrics::{ReplicationKind, ReplicationRunOutcome, ReplicationRunTrigger};
use crate::naming::short_hash;
use crate::pool::slot_gate_is_false;
use crate::snapshot::job_terminal_state;

/// Every kind-specific string the shared plumbing needs, resolved by an
/// exhaustive `match` on [`ReplicationKind`] so a third replication kind cannot
/// compile until it states its own Job labels, name token, and CLI command.
struct KindStrings {
    /// `COMPONENT_LABEL` value on this kind's mover Jobs.
    component: &'static str,
    /// Label tying a mover Job back to its owning CR.
    instance_label: &'static str,
    /// Job-name infix for a MANUAL run — distinct from the cron infix
    /// (`repl`/`srepl`) so a manual run at second X can never collide with a
    /// cron slot at second X, and distinct BETWEEN kinds so two same-named CRs
    /// of different kinds in one namespace never collide either.
    manual_token: &'static str,
    /// The `kubectl kopiur` invocation that stamps a run request for this kind,
    /// quoted verbatim in the malformed-annotation fix hint.
    run_command: &'static str,
}

fn strings(kind: ReplicationKind) -> KindStrings {
    match kind {
        ReplicationKind::Repository => KindStrings {
            component: REPLICATION_COMPONENT,
            instance_label: REPLICATION_INSTANCE_LABEL,
            manual_token: "mrepl",
            run_command: "kubectl kopiur replication run --kind repository",
        },
        ReplicationKind::Snapshot => KindStrings {
            component: SNAPSHOT_REPLICATION_COMPONENT,
            instance_label: SNAPSHOT_REPLICATION_INSTANCE_LABEL,
            manual_token: "msrepl",
            run_command: "kubectl kopiur replication run --kind snapshot",
        },
    }
}

/// The single-flight / counting label selector for one CR's mover Jobs.
pub fn job_selector(kind: ReplicationKind, cr_name: &str) -> String {
    let s = strings(kind);
    format!(
        "{COMPONENT_LABEL}={},{}={cr_name}",
        s.component, s.instance_label
    )
}

/// An UNHANDLED run request: the annotation value verbatim (what
/// `status.manualRun.requestedAt` pins) plus the instant it parses to (the
/// slot the Job runs for).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    /// The `run-requested` annotation value exactly as the user wrote it.
    pub raw: String,
    /// The parsed request instant, used as the run's slot.
    pub at: DateTime<Utc>,
}

/// What the `run-requested` annotation asks for, when there is an UNHANDLED
/// request. Pure apart from the version-skew warning (ADR §5.2).
///
/// `None` when no annotation is present, or when `manual` already TERMINALLY
/// answers this exact `requestedAt`. `Pending` and `Running` are deliberately
/// not answers: `Pending` means the request was recorded while the replication
/// was suspended and still owes a run, and `Running` means a Job exists or
/// existed — the reconcile body resolves that separately. Unparseable values
/// are validation errors (the annotation is user input).
///
/// `kind_label`/`namespace`/`name` are only used to NAME an unreadable stored
/// phase before it is overwritten (see [`crate::io::warn_unreadable_phase`]) —
/// re-driving is idempotent here because the Job name is keyed on the request
/// timestamp, so the self-heal is right; it must simply never be silent.
pub fn manual_run_request(
    kind: ReplicationKind,
    annotations: Option<&BTreeMap<String, String>>,
    manual: Option<&ReplicationManualRunStatus>,
    namespace: &str,
    name: &str,
) -> Result<Option<RunRequest>> {
    let Some(raw) = annotations.and_then(|a| a.get(RUN_REQUESTED_ANNOTATION)) else {
        return Ok(None);
    };
    // Dedupe BEFORE parsing: a terminally-answered request stays answered even
    // if the stored value is later re-written to something unparseable.
    if manual.is_some_and(|m| m.answers(raw)) {
        return Ok(None);
    }
    // Not deduped, and the recorded phase is one this build cannot read: the
    // request will be re-driven and `status.manualRun.phase` overwritten. Name
    // it first — the overwrite is the deliberate self-heal for a driving
    // reconciler, never a silent one.
    if let Some(p) = manual
        .filter(|m| m.requested_at.as_deref() == Some(raw.as_str()))
        .and_then(|m| m.phase.as_ref())
        .filter(|p| p.is_unknown())
    {
        crate::io::warn_unreadable_phase(
            &format!("{} (manualRun)", kind.as_str()),
            namespace,
            name,
            p.label(),
        );
    }
    // Shared parse (also enforced at admission by the webhook) — one
    // validator, two callers.
    match kopiur_api::common::parse_run_requested_at(annotations, strings(kind).run_command) {
        Ok(Some(at)) => Ok(Some(RunRequest {
            raw: raw.clone(),
            at,
        })),
        // Unreachable in practice (the key is present, we read it above), but
        // "no request" is the only honest answer if it ever happens.
        Ok(None) => Ok(None),
        Err(msg) => Err(Error::Validation(msg)),
    }
}

/// Deterministic, ≤52-char, DNS-1123-safe Job name for a MANUAL replication
/// run: `<cr>-<token>-<unix_request>` (truncate + hash long CR names, exactly
/// like the per-slot cron names).
///
/// Keying on the REQUEST timestamp is what makes re-driving safe: the same
/// request always resolves to the same Job, so an interrupted reconcile
/// re-observes the Job it already created instead of launching a second one.
pub fn manual_replication_job_name(
    kind: ReplicationKind,
    cr: &str,
    requested: DateTime<Utc>,
) -> String {
    const MAX: usize = 52;
    let suffix = format!("-{}-{}", strings(kind).manual_token, requested.timestamp());
    let budget = MAX.saturating_sub(suffix.len());
    if cr.len() <= budget {
        format!("{cr}{suffix}")
    } else {
        let hash = short_hash(cr); // 8 hex chars
        let keep = budget.saturating_sub(hash.len() + 1); // room for "-<hash>"
        let head: String = cr.chars().take(keep).collect();
        format!("{head}-{hash}{suffix}")
    }
}

/// `status.manualRun` for a request in one phase. Terminal phases stamp
/// `completedAt`; the in-flight ones deliberately do not.
pub fn manual_run_status(
    request: &RunRequest,
    phase: ReplicationManualRunPhase,
    now: DateTime<Utc>,
) -> ReplicationManualRunStatus {
    // Exhaustive: a new phase must state whether it is a completion instant.
    let completed_at = match &phase {
        ReplicationManualRunPhase::Succeeded | ReplicationManualRunPhase::Failed => {
            Some(now.to_rfc3339())
        }
        ReplicationManualRunPhase::Pending
        | ReplicationManualRunPhase::Running
        | ReplicationManualRunPhase::Unknown(_) => None,
    };
    ReplicationManualRunStatus {
        requested_at: Some(request.raw.clone()),
        phase: Some(phase),
        completed_at,
    }
}

/// Whether `manual` already records THIS request as `Running` — the signal that
/// a Job existed and was TTL-reaped before its outcome could be observed.
pub fn recorded_running(manual: Option<&ReplicationManualRunStatus>, request: &RunRequest) -> bool {
    manual.is_some_and(|m| {
        m.requested_at.as_deref() == Some(request.raw.as_str())
            && m.phase
                .as_ref()
                .is_some_and(ReplicationManualRunPhase::implies_job_launched)
    })
}

/// What `status.manualRun` a SUSPENDED replication should write for an
/// unanswered request — `None` meaning "write nothing, leave what is recorded
/// standing".
///
/// `Pending` is only correct for a request whose Job has NOT been launched.
/// When `suspend` is flipped while the requested Job is already in flight the
/// recorded phase is `Running`, and overwriting it with `Pending` would be
/// wrong twice over:
///
/// * it LIES — `Pending` is documented (and reported by `kubectl kopiur
///   replication run --wait`) as "recorded but not started"; and
/// * it defeats the lost-outcome guard. [`recorded_running`] is the only
///   evidence that a Job ever existed, so once it is overwritten a Job that
///   TTL-reaps during the suspension (1h) leaves no trace, and the first
///   reconcile after the resume silently RE-RUNS side-effectful work instead of
///   reporting `ManualRunOutcomeLost`.
///
/// The suspension is still surfaced either way — [`suspended_report`] fires its
/// distinct `Ready` reason on the presence of the request, not on this write.
pub fn suspended_manual_run(
    request: &RunRequest,
    manual: Option<&ReplicationManualRunStatus>,
    now: DateTime<Utc>,
) -> Option<ReplicationManualRunStatus> {
    if recorded_running(manual, request) {
        return None;
    }
    Some(manual_run_status(
        request,
        ReplicationManualRunPhase::Pending,
        now,
    ))
}

/// The Job annotations a replication mover Job carries for one run: the slot it
/// runs (RFC3339, the caller's own annotation key) and the trigger that asked
/// for it (so the outcome metric can attribute cron vs manual without the CR).
pub fn run_job_annotations(
    slot_annotation: &str,
    slot: DateTime<Utc>,
    trigger: ReplicationRunTrigger,
) -> BTreeMap<String, String> {
    let mut annotations = BTreeMap::new();
    annotations.insert(slot_annotation.to_string(), slot.to_rfc3339());
    annotations.insert(
        RUN_TRIGGER_ANNOTATION.to_string(),
        trigger.as_str().to_string(),
    );
    annotations
}

/// A stall a replication must report on THIS reconcile's SINGLE `Ready` write.
///
/// Two writers of the `Ready` condition in one reconcile do not merge — the
/// second rebuilds the condition array from the object it was handed, which
/// still predates the first, so it silently erases it. Worse, each write wakes
/// the watch, so a pair that disagrees (a stall, then "Idle") flaps forever
/// instead of settling. So the requested-run paths do not write `Ready`
/// themselves: they hand back what to say, and the scheduling flow folds it
/// into the one write it was going to make anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStall {
    /// The `Ready` condition `reason`.
    pub reason: &'static str,
    /// The `Ready` condition `message`.
    pub message: String,
}

impl RunStall {
    /// A stall with a fixed message.
    pub fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

/// What the idle (nothing-due) arm should report: the stall a requested run
/// left behind, or the ordinary "reconciled, waiting for the next slot".
///
/// The stall wins because it is the actionable half — an idle replication is
/// the *normal* state and says nothing a user needs, whereas a failed requested
/// run or a typo'd annotation is precisely what they are looking for.
pub fn idle_report(stall: Option<&RunStall>) -> (bool, &'static str, &str) {
    match stall {
        Some(s) => (false, s.reason, s.message.as_str()),
        None => (true, "Idle", "idle; waiting for the next scheduled run"),
    }
}

/// The `Ready` condition `(reason, message)` a SUSPENDED replication reports,
/// given whether an unanswered run request is waiting. Shared so both kinds say
/// the same thing, and pure so what a suspended CR tells the operator is
/// pinned by a test rather than by two hand-copied string literals.
///
/// The reason DIFFERS between the two cases on purpose: `patch_ready_if_changed`
/// is transition-guarded on `(status, reason)`, so a request arriving at an
/// already-suspended replication would otherwise write nothing and the user
/// would see their request vanish.
pub fn suspended_report(pending_request: bool) -> (&'static str, &'static str) {
    if pending_request {
        (
            "SuspendedWithPendingRun",
            "run requested, but replication is suspended (spec.suspend); it runs once \
             resumed",
        )
    } else {
        ("Suspended", "replication is suspended (spec.suspend)")
    }
}

/// The phase a replication that was suspended and is now RESUMED should show
/// in place of its stale `Suspended` (#689 finding 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumedPhase {
    /// A run Job is in flight.
    Replicating,
    /// Waiting for its next slot (or a gate) — no Job yet.
    Pending,
}

impl ResumedPhase {
    /// The wire label — identical for both replication kinds' phase enums.
    pub fn label(self) -> &'static str {
        match self {
            Self::Replicating => "Replicating",
            Self::Pending => "Pending",
        }
    }
}

/// **Pure.** The `status.phase` rewrite a resumed replication owes, if any.
///
/// The suspended arm stamps `phase: Suspended`, but nothing on the resumed
/// path wrote a phase until the mover stamped `Succeeded`/`Failed` at the END
/// of the next run — so after `suspend: false` the CR (and every UI surface
/// reading it) kept saying `Suspended` through a whole schedule interval and
/// the run itself (#689). Only a phase that is `Suspended` while the spec is
/// NOT suspended is rewritten; every other phase is left to its owner.
pub fn resumed_phase(
    spec_suspended: bool,
    phase_is_suspended: bool,
    has_active_job: bool,
) -> Option<ResumedPhase> {
    match (spec_suspended, phase_is_suspended, has_active_job) {
        // Still suspended, or the phase is not the stale one: nothing to fix.
        (true, _, _) | (false, false, _) => None,
        (false, true, true) => Some(ResumedPhase::Replicating),
        (false, true, false) => Some(ResumedPhase::Pending),
    }
}

/// Rewrite a stale `phase: Suspended` to `fix` — but ONLY if the stored phase
/// is still `Suspended` at write time. Returns whether the rewrite landed.
///
/// A compare-and-swap, not a merge patch (PR #706 review): the caller decided
/// on the heal from the reconcile's copy of the object, and a run Job that was
/// in flight across the resume can stamp `Succeeded`/`Failed` between that
/// read and this write. An unconditional write replaced the mover's result
/// with `Pending`/`Replicating`, and neither the successful-Job branch nor the
/// idle branch restores a terminal phase, so the CR misreported until another
/// run ended. The JSON-patch `test` op makes the apiserver refuse the write
/// (422) when the phase moved on; that loss is the correct outcome, not an
/// error — the newer phase belongs to its writer.
pub async fn heal_stale_suspended_phase<K>(
    api: &Api<K>,
    name: &str,
    fix: ResumedPhase,
) -> Result<bool>
where
    K: kube::Resource + serde::de::DeserializeOwned + Clone + std::fmt::Debug,
{
    let ops: json_patch::Patch = serde_json::from_value(serde_json::json!([
        { "op": "test", "path": "/status/phase", "value": "Suspended" },
        { "op": "replace", "path": "/status/phase", "value": fix.label() },
    ]))?;
    let pp = PatchParams {
        field_manager: Some(crate::io::FIELD_MANAGER.to_string()),
        ..PatchParams::default()
    };
    match api.patch_status(name, &pp, &Patch::Json::<()>(ops)).await {
        Ok(_) => Ok(true),
        // The `test` op failed: the phase is no longer `Suspended`.
        Err(kube::Error::Api(ae)) if ae.code == 422 => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Observe-only bookkeeping for a HELD replication (its repository suspended
/// or not Ready, or the CR itself `spec.suspend`): when the pending request is
/// recorded `Running` and its manual Job has since settled, answer
/// `status.manualRun` `Succeeded`/`Failed`. Launches nothing, writes no
/// `Ready` condition.
///
/// The held paths return before the kind's `handle_manual_run`, so without
/// this a Job that finished during a hold longer than its TTL left the request
/// `Running`, and the resumed reconcile reported `ManualRunOutcomeLost` for a
/// run that worked (PR #706 review). A Job that is still running, or already
/// gone, is left to the resumed path. `current_status` is the CR's `.status`
/// as JSON, so a repeat observation is a no-op write.
#[allow(clippy::too_many_arguments)]
pub async fn observe_settled_manual_run<K>(
    api: &Api<K>,
    job_api: &Api<Job>,
    kind: ReplicationKind,
    namespace: &str,
    name: &str,
    annotations: Option<&BTreeMap<String, String>>,
    manual: Option<&ReplicationManualRunStatus>,
    current_status: Option<&serde_json::Value>,
) -> Result<()>
where
    K: kube::Resource + serde::de::DeserializeOwned + Clone + std::fmt::Debug,
{
    // A malformed request is surfaced by the unheld path; nothing to observe.
    let Ok(Some(request)) = manual_run_request(kind, annotations, manual, namespace, name) else {
        return Ok(());
    };
    if !recorded_running(manual, &request) {
        return Ok(());
    }
    let job_name = manual_replication_job_name(kind, name, request.at);
    let Some(job) = job_api.get_opt(&job_name).await? else {
        return Ok(());
    };
    let phase = match job_terminal_state(&job) {
        Some(true) => ReplicationManualRunPhase::Succeeded,
        Some(false) => ReplicationManualRunPhase::Failed,
        None => return Ok(()),
    };
    crate::io::patch_status_if_changed(
        api,
        name,
        current_status,
        serde_json::json!({ "manualRun": manual_run_status(&request, phase, Utc::now()) }),
    )
    .await?;
    Ok(())
}

/// **Pure.** What holds a replication run behind ONE endpoint repository's
/// availability (#689). Shared by both replication kinds: `None` = that
/// endpoint is usable; `Some((reason, message))` = report `Ready=Reconciling`
/// with that reason and wait. `not_ready` is the caller's existing
/// endpoint-specific `(reason, message)` for a not-Ready repository (so
/// `kubectl describe` keeps naming which end holds the run); a SUSPENDED
/// endpoint always reports `RepositorySuspended`. Exhaustive over
/// [`crate::io::RepositoryAvailability`].
pub fn replication_repository_hold(
    availability: crate::io::RepositoryAvailability,
    not_ready: (&'static str, &'static str),
    repo_ref: &kopiur_api::common::RepositoryRef,
    namespace: &str,
) -> Option<(&'static str, String)> {
    match availability {
        crate::io::RepositoryAvailability::Available => None,
        crate::io::RepositoryAvailability::NotReady => Some((not_ready.0, not_ready.1.to_string())),
        crate::io::RepositoryAvailability::Suspended => Some((
            crate::consts::REPOSITORY_SUSPENDED_REASON,
            crate::io::repository_suspended_message(repo_ref, namespace),
        )),
    }
}

// --- the source repository's mover-Job pool gate ------------------------------

/// The pool gate's verdict for one replication reconcile — the replication peer
/// of `crate::snapshot::PoolGate`.
#[derive(Debug)]
pub enum ReplicationPoolGate {
    /// Spawn the run. `heal` is `true` when a standing
    /// `RepositorySlotAvailable=False` must be flipped once the Job exists.
    Admit {
        /// Whether a previous park is still recorded and must be cleared.
        heal: bool,
        /// This run's pool reservation, held until the replication Job exists.
        /// `None` only when the pool is uncapped — nothing to protect.
        reservation: Option<crate::pool::AdmissionGuard>,
    },
    /// Held behind the source repository's pool. Return this `Action` and spawn
    /// nothing: no Job, no credential projection, no destination Secret checks.
    Parked(Action),
}

/// Consult the SOURCE repository's mover-Job pool before spawning a replication
/// run, parking the CR when it is full.
///
/// The SOURCE is the right pool for both replication kinds: a `sync-to` mirror
/// and a `snapshot migrate` both READ the source repository, and it is that
/// backend (and the bandwidth to it) a `maxConcurrentJobs` protects. A
/// `RepositoryReplication`'s destination is an inline `Backend` with no
/// repository object to have a pool at all.
///
/// Call it immediately before the spawn — every step of the spawn path has
/// external side effects (credential projection into the run namespace, a
/// destination-Secret presence check) that a parked run must not perform, since
/// it may sit queued for an unbounded time.
///
/// `conditions` is the CR's current condition array; `kind`/`name` seed the
/// deterministic requeue jitter. `namespace`/`job_name` are the identity of the
/// Job this run is about to spawn — the reservation the ledger holds until that
/// Job exists is keyed on it, so it MUST be the name actually passed to the
/// spawn below (cron and manual runs use different Job names).
#[allow(clippy::too_many_arguments)]
pub async fn replication_pool_gate<K>(
    ctx: &Context,
    api: &Api<K>,
    obj: &K,
    kind: &str,
    name: &str,
    namespace: &str,
    job_name: &str,
    source: &crate::io::ResolvedRepository,
    conditions: &[Condition],
) -> Result<ReplicationPoolGate>
where
    K: kube::Resource<DynamicType = ()>
        + Clone
        + serde::de::DeserializeOwned
        + std::fmt::Debug
        + serde::Serialize,
{
    let caps = crate::pool::PoolCaps {
        repo: kopiur_api::consts::effective_max_concurrent_jobs(source.concurrency.as_ref()),
        global: ctx.max_concurrent_jobs,
    };
    let pinned = source.repository_ref();
    let heal = slot_gate_is_false(conditions);
    match crate::pool::admit_or_park(
        ctx,
        &crate::naming::repo_label(&pinned),
        &crate::pool::job_key(namespace, job_name),
        crate::pool::PoolClass::Replication,
        caps,
    )
    .await?
    {
        crate::pool::LedgerVerdict::Admit { reservation } => {
            Ok(ReplicationPoolGate::Admit { heal, reservation })
        }
        crate::pool::LedgerVerdict::Park {
            repo_live,
            global_live,
        } => {
            let message = crate::pool::waiting_for_slot_message(
                crate::io::repo_kind_str(pinned.kind),
                &crate::pool::repo_display(&pinned),
                repo_live,
                global_live,
                caps,
            );
            let updated = crate::io::upsert_condition(
                conditions,
                crate::consts::REPOSITORY_SLOT_AVAILABLE_CONDITION,
                false,
                crate::consts::WAITING_FOR_SLOT_REASON,
                &message,
                obj.meta().generation,
            );
            // Only the condition — the `Ready` condition, phase and
            // `lastReplicated` are deliberately left alone: a queued run is not
            // a failure and must not flip a health check. `upsert_condition`
            // preserves `lastTransitionTime` while status+message are unchanged,
            // so a steady-state park re-patches identical bytes.
            let current =
                serde_json::to_value(serde_json::json!({ "conditions": conditions })).ok();
            let wrote = crate::io::patch_status_if_changed(
                api,
                name,
                current.as_ref(),
                serde_json::json!({ "conditions": updated }),
            )
            .await?;
            // Transition-gated: fire once on entering the queue, not on every
            // re-check of a queue that may last minutes.
            if wrote && !heal {
                crate::io::publish_normal_event(
                    ctx,
                    obj,
                    crate::consts::WAITING_FOR_SLOT_REASON,
                    "WaitForRepositorySlot",
                    &message,
                )
                .await;
                tracing::info!(
                    replication = %name,
                    repository = %pinned.name,
                    repo_live,
                    global_live,
                    "replication waiting for a mover slot on the source repository"
                );
            }
            Ok(ReplicationPoolGate::Parked(Action::requeue(
                crate::pool::pool_wait_requeue(kind, obj.meta().namespace.as_deref(), name),
            )))
        }
    }
}

/// Clear a standing `RepositorySlotAvailable=False` once the run's Job exists.
///
/// Seeded from a LIVE re-read, not the reconcile's (already stale) copy: a
/// `conditions` patch replaces the whole array, and the spawn path between the
/// gate and here can have written conditions of its own. Best-effort by
/// contract — the Job is already created, and failing the reconcile over a
/// cosmetic condition flip would re-run the gate against a pool the new Job is
/// now in.
///
/// **Also called from the in-flight arm**, and that is not redundant. Once the
/// Job exists the reconcile returns at the `job_api.get_opt` branch and never
/// reaches the gate again, so a heal that failed at spawn time would otherwise
/// leave the CR advertising `False` for a whole schedule interval — until the
/// NEXT run's spawn healed it. Re-attempting on the in-flight arm closes that
/// window to one requeue. It costs nothing when there is nothing to heal: the
/// caller computes `heal` from conditions it already holds, and this returns
/// before any IO when it is `false`.
pub async fn heal_replication_slot_condition<K>(
    api: &Api<K>,
    obj: &K,
    name: &str,
    source: &crate::io::ResolvedRepository,
    conditions_of: fn(&K) -> Vec<Condition>,
    heal: bool,
) where
    K: kube::Resource<DynamicType = ()>
        + Clone
        + serde::de::DeserializeOwned
        + std::fmt::Debug
        + serde::Serialize,
{
    if !heal {
        return;
    }
    let Some(live) = crate::io::live_conditions_source(api, name, obj).await else {
        return;
    };
    let existing = conditions_of(&live);
    if !slot_gate_is_false(&existing) {
        return;
    }
    let pinned = source.repository_ref();
    let updated = crate::io::upsert_condition(
        &existing,
        crate::consts::REPOSITORY_SLOT_AVAILABLE_CONDITION,
        true,
        crate::consts::SLOT_ACQUIRED_REASON,
        &crate::pool::slot_acquired_message(
            crate::io::repo_kind_str(pinned.kind),
            &crate::pool::repo_display(&pinned),
        ),
        obj.meta().generation,
    );
    if let Err(e) =
        crate::io::patch_status(api, name, serde_json::json!({ "conditions": updated })).await
    {
        tracing::debug!(
            replication = %name,
            error = %e,
            "could not clear RepositorySlotAvailable; will retry"
        );
    }
}

/// What one reconcile learned from listing this CR's mover Jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunObservation {
    /// Whether any non-terminal mover Job is owned by this CR (the G3
    /// single-flight gate: never two replication Jobs for one CR).
    pub has_active: bool,
}

/// The metric attribution for one terminal Job, or `None` when it was already
/// counted. Pure, so the whole decision is unit-testable without a cluster.
///
/// The trigger comes from the Job's own [`RUN_TRIGGER_ANNOTATION`]; a Job
/// launched by an OLDER kopiur carries none, and is attributed to `cron` —
/// that build had no manual path at all, so `cron` is not a guess, it is the
/// only run it could have been.
pub fn run_to_count(job: &Job) -> Option<(ReplicationRunTrigger, ReplicationRunOutcome)> {
    let annotations = job.metadata.annotations.as_ref();
    if annotations.is_some_and(|a| a.contains_key(RUN_COUNTED_ANNOTATION)) {
        return None;
    }
    let outcome = match job_terminal_state(job)? {
        true => ReplicationRunOutcome::Succeeded,
        false => ReplicationRunOutcome::Failed,
    };
    let trigger = annotations
        .and_then(|a| a.get(RUN_TRIGGER_ANNOTATION))
        .and_then(|v| ReplicationRunTrigger::parse(v))
        .unwrap_or(ReplicationRunTrigger::Cron);
    Some((trigger, outcome))
}

/// List this CR's mover Jobs once: count every terminal run not yet counted
/// (stamping the durable marker first, so a crash between the two re-counts at
/// most one run rather than losing it), and report the single-flight gate.
///
/// Best-effort on the counting side by contract — a failed marker patch logs
/// and skips the increment, because a metric must never fail a reconcile that
/// would otherwise make progress. The single-flight answer is NOT best-effort:
/// a failed LIST propagates.
pub async fn observe_and_count_runs(
    ctx: &Context,
    job_api: &Api<Job>,
    kind: ReplicationKind,
    cr_name: &str,
) -> Result<RunObservation> {
    let selector = job_selector(kind, cr_name);
    let jobs = job_api
        .list(&ListParams::default().labels(&selector))
        .await?;
    let mut has_active = false;
    for job in &jobs.items {
        if job_terminal_state(job).is_none() {
            has_active = true;
            continue;
        }
        let Some((trigger, outcome)) = run_to_count(job) else {
            continue;
        };
        let Some(job_name) = job.metadata.name.as_deref() else {
            continue;
        };
        // Stamp first, count second. The ordering is a deliberate bias, not a
        // guarantee: a crash in the window between the two LOSES that run's
        // increment forever (the Job is now marked counted and no later
        // reconcile will revisit it). Counting first and stamping second would
        // trade that for a DOUBLE count on the same crash. Undercount wins —
        // this is an in-process counter that resets on every operator restart
        // anyway, so a rate/alert built on it already tolerates a missing
        // increment around a restart, whereas a phantom `outcome="failed"`
        // would page someone for a run that never failed.
        let body = Patch::Merge(serde_json::json!({
            "metadata": { "annotations": { RUN_COUNTED_ANNOTATION: outcome.as_str() } }
        }));
        match job_api
            .patch(job_name, &PatchParams::default(), &body)
            .await
        {
            Ok(_) => ctx.metrics.inc_replication_run(kind, trigger, outcome),
            Err(e) => tracing::debug!(
                replication = %cr_name,
                job = %job_name,
                error = %e,
                "could not mark the run as counted; will retry the run metric next reconcile"
            ),
        }
    }
    Ok(RunObservation { has_active })
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};
    use kube::api::ObjectMeta;

    fn annotated(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn requested(raw: &str) -> BTreeMap<String, String> {
        annotated(&[(RUN_REQUESTED_ANNOTATION, raw)])
    }

    fn status(raw: &str, phase: ReplicationManualRunPhase) -> ReplicationManualRunStatus {
        ReplicationManualRunStatus {
            requested_at: Some(raw.to_string()),
            phase: Some(phase),
            completed_at: None,
        }
    }

    fn job(annotations: Option<BTreeMap<String, String>>, condition: Option<(&str, &str)>) -> Job {
        Job {
            metadata: ObjectMeta {
                name: Some("j".into()),
                annotations,
                ..Default::default()
            },
            status: condition.map(|(type_, status)| JobStatus {
                conditions: Some(vec![JobCondition {
                    type_: type_.into(),
                    status: status.into(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    const RAW: &str = "2026-06-11T12:00:00Z";

    #[test]
    fn no_annotation_is_no_request() {
        for kind in [ReplicationKind::Repository, ReplicationKind::Snapshot] {
            assert_eq!(
                manual_run_request(kind, None, None, "ns", "r").expect("ok"),
                None
            );
            assert_eq!(
                manual_run_request(kind, Some(&BTreeMap::new()), None, "ns", "r").expect("ok"),
                None
            );
        }
    }

    #[test]
    fn a_fresh_request_is_returned_verbatim_and_parsed() {
        let a = requested(RAW);
        let req = manual_run_request(ReplicationKind::Repository, Some(&a), None, "ns", "r")
            .expect("ok")
            .expect("requested");
        assert_eq!(req.raw, RAW, "status pins the value the user wrote");
        assert_eq!(req.at.to_rfc3339(), "2026-06-11T12:00:00+00:00");
    }

    #[test]
    fn dedupe_answers_only_terminal_phases_of_the_same_timestamp() {
        let a = requested(RAW);
        let ask = |st: Option<&ReplicationManualRunStatus>| {
            manual_run_request(ReplicationKind::Snapshot, Some(&a), st, "ns", "r").expect("ok")
        };
        // Terminal, same timestamp: answered, so the request is not re-driven.
        for terminal in [
            ReplicationManualRunPhase::Succeeded,
            ReplicationManualRunPhase::Failed,
        ] {
            assert!(
                ask(Some(&status(RAW, terminal.clone()))).is_none(),
                "{terminal:?} answers the request"
            );
        }
        // Not terminal: still owed. `Pending` is the suspended case — the whole
        // reason a request survives a suspend instead of being dropped.
        for open in [
            ReplicationManualRunPhase::Pending,
            ReplicationManualRunPhase::Running,
            ReplicationManualRunPhase::Unknown("Queued".into()),
        ] {
            assert!(
                ask(Some(&status(RAW, open.clone()))).is_some(),
                "{open:?} does not answer the request"
            );
        }
        // A terminal answer to a DIFFERENT timestamp answers nothing: a new
        // annotation value is a new run.
        assert!(
            ask(Some(&status(
                "2026-06-11T13:00:00Z",
                ReplicationManualRunPhase::Succeeded
            )))
            .is_some()
        );
    }

    #[test]
    fn an_answered_request_stays_answered_even_if_the_value_is_later_garbage() {
        // Dedupe runs BEFORE parsing on purpose: re-writing the annotation to
        // garbage must not resurrect a finished run (it would also be refused
        // at admission, but the controller cannot rely on that).
        let a = requested("yesterday");
        let answered = status("yesterday", ReplicationManualRunPhase::Succeeded);
        assert!(
            manual_run_request(
                ReplicationKind::Repository,
                Some(&a),
                Some(&answered),
                "ns",
                "r"
            )
            .expect("ok")
            .is_none()
        );
    }

    #[test]
    fn garbage_is_a_validation_error_naming_this_kinds_run_command() {
        let a = requested("yesterday");
        for (kind, want) in [
            (ReplicationKind::Repository, "--kind repository"),
            (ReplicationKind::Snapshot, "--kind snapshot"),
        ] {
            let err = manual_run_request(kind, Some(&a), None, "ns", "r").expect_err("garbage");
            let msg = err.to_string();
            assert!(msg.contains("must be an RFC3339 timestamp"), "{msg}");
            assert!(msg.contains("kubectl kopiur replication run"), "{msg}");
            assert!(msg.contains(want), "{msg}");
        }
    }

    #[test]
    fn manual_job_names_are_deterministic_bounded_and_collision_free() {
        let at = DateTime::parse_from_rfc3339("2026-06-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let repo = manual_replication_job_name(ReplicationKind::Repository, "offsite", at);
        let snap = manual_replication_job_name(ReplicationKind::Snapshot, "offsite", at);
        assert_eq!(
            repo,
            manual_replication_job_name(ReplicationKind::Repository, "offsite", at),
            "deterministic: the same request re-observes its own Job"
        );
        // Distinct from each other AND from the cron per-slot names of both
        // kinds at the same second (`-repl-`/`-srepl-`).
        assert_ne!(repo, snap);
        for n in [&repo, &snap] {
            assert!(n.len() <= 52, "{n}");
            assert_ne!(n.as_str(), &format!("offsite-repl-{}", at.timestamp()));
            assert_ne!(n.as_str(), &format!("offsite-srepl-{}", at.timestamp()));
        }
        let long = "a-very-long-repository-replication-name-blowing-the-dns-budget";
        for kind in [ReplicationKind::Repository, ReplicationKind::Snapshot] {
            assert!(manual_replication_job_name(kind, long, at).len() <= 52);
        }
    }

    #[test]
    fn only_terminal_phases_stamp_a_completion_instant() {
        let now = Utc::now();
        let req = RunRequest {
            raw: RAW.into(),
            at: now,
        };
        for phase in [
            ReplicationManualRunPhase::Succeeded,
            ReplicationManualRunPhase::Failed,
        ] {
            let st = manual_run_status(&req, phase.clone(), now);
            assert_eq!(st.requested_at.as_deref(), Some(RAW));
            assert!(st.completed_at.is_some(), "{phase:?} is terminal");
        }
        for phase in [
            ReplicationManualRunPhase::Pending,
            ReplicationManualRunPhase::Running,
        ] {
            let st = manual_run_status(&req, phase.clone(), now);
            assert!(st.completed_at.is_none(), "{phase:?} has not completed");
            // #394: and it must SERIALIZE as an explicit null, not vanish — the
            // merge-patch only clears a previous run's stamp when the key is
            // present with a null value (the apiserver then deletes the key or
            // stores the null; either way no stale timestamp survives).
            let json = serde_json::to_value(&st).unwrap();
            assert_eq!(
                json.get("completedAt"),
                Some(&serde_json::Value::Null),
                "{phase:?} must clear a stale completedAt; got {json}"
            );
        }
    }

    #[test]
    fn recorded_running_pins_this_request_only() {
        let req = RunRequest {
            raw: RAW.into(),
            at: Utc::now(),
        };
        assert!(recorded_running(
            Some(&status(RAW, ReplicationManualRunPhase::Running)),
            &req
        ));
        assert!(!recorded_running(
            Some(&status(RAW, ReplicationManualRunPhase::Pending)),
            &req
        ));
        assert!(!recorded_running(
            Some(&status(
                "2026-06-11T13:00:00Z",
                ReplicationManualRunPhase::Running
            )),
            &req
        ));
        assert!(!recorded_running(None, &req));
    }

    #[test]
    fn job_annotations_carry_the_slot_and_the_trigger() {
        let at = DateTime::parse_from_rfc3339("2026-06-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let a = run_job_annotations("k/slot", at, ReplicationRunTrigger::Manual);
        assert_eq!(
            a.get("k/slot").map(String::as_str),
            Some("2026-06-11T12:00:00+00:00")
        );
        assert_eq!(
            a.get(RUN_TRIGGER_ANNOTATION).map(String::as_str),
            Some("manual")
        );
    }

    #[test]
    fn run_to_count_attributes_terminal_uncounted_jobs_only() {
        // In flight: nothing to count yet.
        assert_eq!(run_to_count(&job(None, None)), None);
        // Terminal, no trigger annotation (a Job from an older kopiur, which
        // had no manual path): attributed to cron, not dropped.
        assert_eq!(
            run_to_count(&job(None, Some(("Complete", "True")))),
            Some((
                ReplicationRunTrigger::Cron,
                ReplicationRunOutcome::Succeeded
            ))
        );
        // Terminal + manual trigger.
        assert_eq!(
            run_to_count(&job(
                Some(annotated(&[(RUN_TRIGGER_ANNOTATION, "manual")])),
                Some(("Failed", "True"))
            )),
            Some((ReplicationRunTrigger::Manual, ReplicationRunOutcome::Failed))
        );
        // An unrecognized trigger value degrades to cron rather than dropping
        // the run out of the series entirely.
        assert_eq!(
            run_to_count(&job(
                Some(annotated(&[(RUN_TRIGGER_ANNOTATION, "wat")])),
                Some(("Complete", "True"))
            )),
            Some((
                ReplicationRunTrigger::Cron,
                ReplicationRunOutcome::Succeeded
            ))
        );
        // Already counted: never again, whatever else it says.
        assert_eq!(
            run_to_count(&job(
                Some(annotated(&[
                    (RUN_TRIGGER_ANNOTATION, "manual"),
                    (RUN_COUNTED_ANNOTATION, "succeeded")
                ])),
                Some(("Complete", "True"))
            )),
            None
        );
    }

    #[test]
    fn suspending_mid_run_leaves_the_running_phase_standing() {
        let req = RunRequest {
            raw: RAW.into(),
            at: Utc::now(),
        };
        let now = Utc::now();

        // The Job is in flight: write NOTHING. Overwriting `Running` with
        // `Pending` would both lie and erase the only evidence a Job exists,
        // which is what `recorded_running` reads to report a lost outcome.
        assert_eq!(
            suspended_manual_run(
                &req,
                Some(&status(RAW, ReplicationManualRunPhase::Running)),
                now
            ),
            None
        );

        // Nothing launched yet — the ordinary case — records Pending.
        for before in [
            None,
            Some(status(RAW, ReplicationManualRunPhase::Pending)),
            // A terminal answer to a DIFFERENT request is not this request's
            // Job, so this request is still un-launched.
            Some(status(
                "2026-06-11T13:00:00Z",
                ReplicationManualRunPhase::Succeeded,
            )),
        ] {
            let written = suspended_manual_run(&req, before.as_ref(), now)
                .expect("an un-launched request records Pending");
            assert_eq!(written.phase, Some(ReplicationManualRunPhase::Pending));
            assert_eq!(written.requested_at.as_deref(), Some(RAW));
            assert!(
                written.completed_at.is_none(),
                "Pending has not completed: {written:?}"
            );
        }
    }

    #[test]
    fn a_job_ttl_reaped_during_a_suspension_is_still_reported_lost() {
        // The whole point of the rule above, as a sequence:
        //   Running -> suspend (no write) -> Job TTL-reaps -> resume.
        let req = RunRequest {
            raw: RAW.into(),
            at: Utc::now(),
        };
        let running = status(RAW, ReplicationManualRunPhase::Running);

        // Suspend leaves it alone…
        assert_eq!(suspended_manual_run(&req, Some(&running), Utc::now()), None);
        // …so the request is STILL owed after the resume…
        let a = requested(RAW);
        assert!(
            manual_run_request(
                ReplicationKind::Repository,
                Some(&a),
                Some(&running),
                "ns",
                "r"
            )
            .expect("ok")
            .is_some()
        );
        // …and the reaped-Job branch still fires, so the post-resume reconcile
        // reports the lost outcome instead of silently re-running.
        assert!(recorded_running(Some(&running), &req));

        // Contrast: had the suspend written Pending, the guard would be blind
        // and the resume would spawn a SECOND run for the same request.
        let clobbered = status(RAW, ReplicationManualRunPhase::Pending);
        assert!(!recorded_running(Some(&clobbered), &req));
    }

    #[test]
    fn the_idle_arm_reports_a_stall_over_the_ordinary_idle_line() {
        // No stall: the ordinary Ready/Idle report.
        let (ready, reason, message) = idle_report(None);
        assert!(ready);
        assert_eq!(reason, "Idle");
        assert!(message.contains("waiting for the next scheduled run"));

        // A stall from a requested run REPLACES it — one Ready writer per
        // reconcile, so the actionable half must be the one that lands.
        let stall = RunStall::new("InvalidRunRequest", "annotation ... is not RFC3339");
        let (ready, reason, message) = idle_report(Some(&stall));
        assert!(!ready);
        assert_eq!(reason, "InvalidRunRequest");
        assert_eq!(message, "annotation ... is not RFC3339");
    }

    #[test]
    fn a_suspended_replication_reports_a_waiting_request_distinctly() {
        let (quiet_reason, quiet_msg) = suspended_report(false);
        let (pending_reason, pending_msg) = suspended_report(true);
        // Distinct reasons: the Ready patch is transition-guarded on
        // (status, reason), so a request landing on an already-suspended
        // replication must move the reason or the user never sees it.
        assert_ne!(quiet_reason, pending_reason);
        assert!(pending_msg.contains("run requested"), "{pending_msg}");
        assert!(pending_msg.contains("suspended"), "{pending_msg}");
        // …and says what unblocks it, per the what/why/fix rule.
        assert!(pending_msg.contains("resumed"), "{pending_msg}");
        assert!(!quiet_msg.contains("run requested"), "{quiet_msg}");
    }

    #[test]
    fn job_selector_is_per_kind_and_per_cr() {
        let repo = job_selector(ReplicationKind::Repository, "offsite");
        let snap = job_selector(ReplicationKind::Snapshot, "offsite");
        assert_ne!(repo, snap, "the two kinds' Jobs must never cross-select");
        assert!(repo.contains(REPLICATION_COMPONENT), "{repo}");
        assert!(repo.ends_with("=offsite"), "{repo}");
        assert!(snap.contains(SNAPSHOT_REPLICATION_COMPONENT), "{snap}");
    }

    #[test]
    fn a_suspended_endpoint_of_either_kind_holds_a_replication_run() {
        use crate::io::RepositoryAvailability as A;
        use kopiur_api::common::{RepositoryKind, RepositoryRef};
        let not_ready = (
            "WaitingForSourceRepository",
            "source repository is not Ready",
        );
        for kind in [
            RepositoryKind::Repository,
            RepositoryKind::ClusterRepository,
        ] {
            let rref = RepositoryRef {
                kind,
                name: "src".into(),
                namespace: None,
            };
            assert_eq!(
                replication_repository_hold(A::Available, not_ready, &rref, "apps"),
                None
            );
            // Not Ready keeps the caller's endpoint-specific reason.
            assert_eq!(
                replication_repository_hold(A::NotReady, not_ready, &rref, "apps"),
                Some((not_ready.0, not_ready.1.to_string()))
            );
            let (reason, msg) =
                replication_repository_hold(A::Suspended, not_ready, &rref, "apps").expect("held");
            assert_eq!(reason, crate::consts::REPOSITORY_SUSPENDED_REASON);
            assert!(
                msg.contains("src") && msg.contains("spec.suspend: false"),
                "{msg}"
            );
        }
    }

    /// #689 finding 2: a replication resumed from `suspend: true` kept
    /// `phase: Suspended` until its next run ENDED. Every combination of the
    /// three inputs is pinned.
    #[test]
    fn a_resumed_replication_replaces_its_stale_suspended_phase() {
        // Still suspended: the suspended arm owns the phase.
        for phase_suspended in [true, false] {
            for active in [true, false] {
                assert_eq!(resumed_phase(true, phase_suspended, active), None);
            }
        }
        // Resumed, but the phase is not the stale one: left to its owner.
        assert_eq!(resumed_phase(false, false, true), None);
        assert_eq!(resumed_phase(false, false, false), None);
        // Resumed with the stale phase: Replicating while a Job runs, else Pending.
        assert_eq!(
            resumed_phase(false, true, true),
            Some(ResumedPhase::Replicating)
        );
        assert_eq!(
            resumed_phase(false, true, false),
            Some(ResumedPhase::Pending)
        );
        // The labels are the wire values of BOTH kinds' phase enums.
        use kopiur_api::common::PhaseLabel;
        assert_eq!(
            ResumedPhase::Replicating.label(),
            kopiur_api::RepositoryReplicationPhase::Replicating.label()
        );
        assert_eq!(
            ResumedPhase::Pending.label(),
            kopiur_api::SnapshotReplicationPhase::Pending.label()
        );
    }

    /// A mock apiserver that records every request (method, content-type,
    /// body) and answers a status PATCH with `patch_code`.
    fn heal_client(
        log: std::sync::Arc<std::sync::Mutex<Vec<(String, String, serde_json::Value)>>>,
        patch_code: u16,
    ) -> kube::Client {
        use http::{Request, Response, StatusCode};
        use kube::client::Body;
        let svc = tower::service_fn(move |req: Request<Body>| {
            let log = log.clone();
            async move {
                let method = req.method().to_string();
                let ctype = req
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                let bytes = http_body_util::BodyExt::collect(req.into_body())
                    .await
                    .expect("body")
                    .to_bytes();
                let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                log.lock().expect("log").push((method, ctype, body));
                let (status, resp) = if patch_code == 200 {
                    (
                        StatusCode::OK,
                        serde_json::json!({
                            "apiVersion": "kopiur.home-operations.com/v1alpha1",
                            "kind": "RepositoryReplication",
                            "metadata": { "name": "r", "namespace": "ns" },
                            "spec": {
                                "sourceRef": { "name": "nas" },
                                "destination": { "filesystem": { "path": "/x" } },
                                "schedule": { "cron": "0 * * * *" }
                            }
                        }),
                    )
                } else {
                    (
                        StatusCode::from_u16(patch_code).expect("code"),
                        serde_json::json!({
                            "kind": "Status", "apiVersion": "v1", "status": "Failure",
                            "message": "the server rejected our request: test operation failed",
                            "reason": "Invalid", "code": patch_code,
                        }),
                    )
                };
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(resp.to_string().into_bytes()))
                        .expect("response"),
                )
            }
        });
        kube::Client::new(svc, "ns")
    }

    /// A mock apiserver for the held manual-run tests: a GET of the Job named
    /// `job` answers with `job_status` (404 when `None`); every PATCH body is
    /// recorded and answered with a minimal RepositoryReplication.
    fn manual_client(
        job: String,
        job_status: Option<serde_json::Value>,
        patches: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) -> kube::Client {
        use http::{Request, Response, StatusCode};
        use kube::client::Body;
        let svc = tower::service_fn(move |req: Request<Body>| {
            let job = job.clone();
            let job_status = job_status.clone();
            let patches = patches.clone();
            async move {
                let method = req.method().to_string();
                let path = req.uri().path().to_string();
                let bytes = http_body_util::BodyExt::collect(req.into_body())
                    .await
                    .expect("body")
                    .to_bytes();
                let not_found = serde_json::json!({
                    "kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "reason": "NotFound", "code": 404, "message": "not found",
                });
                let (status, body) = match method.as_str() {
                    "GET" if path.ends_with(&format!("/jobs/{job}")) => match &job_status {
                        Some(st) => (
                            StatusCode::OK,
                            serde_json::json!({
                                "apiVersion": "batch/v1", "kind": "Job",
                                "metadata": { "name": job, "namespace": "ns" },
                                "status": st,
                            }),
                        ),
                        None => (StatusCode::NOT_FOUND, not_found),
                    },
                    "PATCH" => {
                        patches
                            .lock()
                            .expect("log")
                            .push(serde_json::from_slice(&bytes).expect("patch body"));
                        (
                            StatusCode::OK,
                            serde_json::json!({
                                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                                "kind": "RepositoryReplication",
                                "metadata": { "name": "r", "namespace": "ns" },
                                "spec": {
                                    "sourceRef": { "name": "nas" },
                                    "destination": { "filesystem": { "path": "/x" } },
                                    "schedule": { "cron": "0 * * * *" }
                                }
                            }),
                        )
                    }
                    _ => (StatusCode::NOT_FOUND, not_found),
                };
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string().into_bytes()))
                        .expect("response"),
                )
            }
        });
        kube::Client::new(svc, "ns")
    }

    /// PR #706 review (Greptile P2): a held replication (its source repository
    /// suspended or not Ready, or the CR itself `spec.suspend`) returned before
    /// `handle_manual_run`, so a manual Job that FINISHED during the hold left
    /// `status.manualRun` at `Running`. Once its TTL reaped it, the resumed
    /// reconcile reported `ManualRunOutcomeLost` for a copy that succeeded.
    /// The held paths must record a settled manual Job's outcome.
    #[tokio::test]
    async fn a_held_replication_still_answers_a_manual_run_whose_job_finished() {
        let raw = "2026-06-11T12:00:00Z";
        let at = chrono::DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc);
        let job = manual_replication_job_name(ReplicationKind::Repository, "r", at);
        let annotations: BTreeMap<String, String> =
            [(RUN_REQUESTED_ANNOTATION.to_string(), raw.to_string())].into();
        let running = ReplicationManualRunStatus {
            requested_at: Some(raw.into()),
            phase: Some(ReplicationManualRunPhase::Running),
            completed_at: None,
        };
        let complete = serde_json::json!({ "conditions": [
            { "type": "Complete", "status": "True" }
        ] });
        let failed = serde_json::json!({ "conditions": [
            { "type": "Failed", "status": "True" }
        ] });

        for (job_status, want) in [
            (Some(complete), Some("Succeeded")),
            (Some(failed), Some("Failed")),
            // Still running, or gone: nothing to record while held (the
            // resumed path owns the lost-outcome verdict).
            (Some(serde_json::json!({ "active": 1 })), None),
            (None, None),
        ] {
            let patches = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let client = manual_client(job.clone(), job_status, patches.clone());
            let api: Api<kopiur_api::RepositoryReplication> = Api::namespaced(client.clone(), "ns");
            let job_api: Api<Job> = Api::namespaced(client, "ns");
            observe_settled_manual_run(
                &api,
                &job_api,
                ReplicationKind::Repository,
                "ns",
                "r",
                Some(&annotations),
                Some(&running),
                None,
            )
            .await
            .expect("observe");
            let patches = patches.lock().expect("log").clone();
            match want {
                Some(phase) => assert!(
                    patches
                        .iter()
                        .any(|p| p["status"]["manualRun"]["phase"] == phase),
                    "want manualRun {phase}: {patches:?}"
                ),
                None => assert!(patches.is_empty(), "no write expected: {patches:?}"),
            }
        }

        // A request that is not recorded `Running` launched nothing to observe.
        let patches = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = manual_client(job, None, patches.clone());
        let api: Api<kopiur_api::RepositoryReplication> = Api::namespaced(client.clone(), "ns");
        let job_api: Api<Job> = Api::namespaced(client, "ns");
        observe_settled_manual_run(
            &api,
            &job_api,
            ReplicationKind::Repository,
            "ns",
            "r",
            Some(&annotations),
            None,
            None,
        )
        .await
        .expect("observe");
        assert!(patches.lock().expect("log").is_empty());
    }

    /// PR #706 review (Greptile P2): the resume heal read `phase: Suspended`
    /// from the reconcile's (cached) copy, then merge-patched `Pending`/
    /// `Replicating` UNCONDITIONALLY — so a mover that wrote `Succeeded`
    /// between that read and the write had its result overwritten, and nothing
    /// restored it until another run finished. The heal must be a
    /// compare-and-swap on the stored phase still being `Suspended`.
    #[tokio::test]
    async fn the_resume_heal_only_rewrites_a_phase_that_is_still_suspended() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let api: Api<kopiur_api::RepositoryReplication> =
            Api::namespaced(heal_client(log.clone(), 200), "ns");
        let healed = heal_stale_suspended_phase(&api, "r", ResumedPhase::Pending)
            .await
            .expect("heal");
        assert!(healed);
        let recorded = log.lock().expect("log").clone();
        assert_eq!(recorded.len(), 1, "exactly one write: {recorded:?}");
        let (method, ctype, body) = &recorded[0];
        assert_eq!(method, "PATCH");
        assert_eq!(
            ctype, "application/json-patch+json",
            "a merge patch cannot be conditional on the stored phase"
        );
        assert_eq!(
            body,
            &serde_json::json!([
                { "op": "test", "path": "/status/phase", "value": "Suspended" },
                { "op": "replace", "path": "/status/phase", "value": "Pending" },
            ])
        );

        // The mover won the race: the stored phase is no longer `Suspended`,
        // so the test op fails (422) — that is the CAS losing, not an error,
        // and the mover's phase stands.
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let api: Api<kopiur_api::RepositoryReplication> =
            Api::namespaced(heal_client(log.clone(), 422), "ns");
        let healed = heal_stale_suspended_phase(&api, "r", ResumedPhase::Replicating)
            .await
            .expect("a lost compare-and-swap is not a reconcile error");
        assert!(!healed);
    }
}
