//! The `Snapshot` reconciler — the heart of the ADR §5.5 thesis.
//!
//! Two paths:
//! 1. **Normal reconcile** (produced backups): add the `kopiur.home-operations.com/snapshot-cleanup`
//!    finalizer, create a mover `Job` + `ConfigMap` (work spec), watch it to a
//!    terminal state, copy stats/phase into `status`, and reap (owner-ref GC).
//! 2. **Deletion** (finalizer present, `deletionTimestamp` set): run the
//!    EXHAUSTIVE [`plan_deletion`] decision, execute its IO, then remove the
//!    finalizer.
//!
//! [`plan_deletion`] is a pure function over [`DeletionFacts`] returning a
//! [`DeletionPlan`]. It is the single most important thing to get right and is
//! exhaustively unit-tested — every `match` over `DeletionPolicy`,
//! `ScheduleDeletePolicy`, `NamespaceDeletePolicy`, `OwnerState`, and
//! `BreakerState` has **no** `_ =>` arm, so a new variant of any of them cannot
//! compile until handled (SKILL thesis).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, ObjectReference, Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::runtime::controller::Action;
use kube::runtime::events::{Event, EventType};
use kube::runtime::reflector::{ObjectRef, Store};
use kube::{Api, Resource, ResourceExt};

use kopiur_api::backend::Backend;
use kopiur_api::common::{FailureBlock, NamespaceDeletePolicy, RepositoryKind, RepositoryRef};
use kopiur_api::snapshot::SnapshotPhase;
use kopiur_api::{DeletionPolicy, Origin, Snapshot, SnapshotPolicy, SnapshotSchedule};

use crate::metrics::{BatchJobOutcome, BatchMemberOutcome, SnapshotDeletionOutcome};
use kopiur_mover::workspec::{
    MoverOptions, MoverWorkSpec, Operation, SnapshotDeleteBatchOp, SnapshotDeleteItem,
    SnapshotPinOp, TargetRef,
};

use crate::config;
use crate::consts::{
    ACKNOWLEDGE_MASS_DELETION_ACTION, ALLOW_PRIVILEGED_MOVER_ACTION, API_VERSION,
    CREDENTIALS_AVAILABLE_CONDITION, CREDENTIALS_PROJECTED_REASON, DELETE_MEMBERS_ANNOTATION,
    DELETE_REPO_LABEL, DELETION_HELD_CONDITION, ENABLE_POLICY_CASCADE_ACTION,
    ENABLE_SCHEDULE_CASCADE_ACTION, FIX_HOOK_ACTION, FIX_SNAPSHOT_STACK_ACTION,
    HOOKS_SUCCEEDED_CONDITION, INHERIT_APPLIED_REASON, INHERIT_FALLBACK_REASON,
    INHERIT_OVERRIDDEN_REASON, INHERIT_PINNED_NO_UID_REASON, INVALID_MASS_DELETION_ACK_REASON,
    MANAGED_BY_LABEL, MANAGED_BY_VALUE, MASS_DELETION_ACKNOWLEDGED_REASON,
    MATCH_WORKLOAD_SECURITY_CONTEXT_ACTION, MOVER_PERMITTED_CONDITION, OP_LABEL,
    OP_SNAPSHOT_DELETE_BATCH, PIN_WORKLOAD_RUN_AS_USER_ACTION,
    PRIVILEGED_MOVER_NOT_PERMITTED_REASON, SECURITY_CONTEXT_COMPATIBLE_CONDITION,
    SECURITY_CONTEXT_COMPATIBLE_REASON, SECURITY_CONTEXT_INHERITED_CONDITION,
    SKIP_SNAPSHOT_CLEANUP_ANNOTATION, SNAPSHOT_CLEANUP_FINALIZER, SNAPSHOT_DELETION_HELD_REASON,
    SNAPSHOT_INCOMPLETE_REASON, SNAPSHOT_RETAINED_ON_POLICY_DELETE_REASON,
    SNAPSHOT_RETAINED_ON_SCHEDULE_DELETE_REASON, SOURCE_STAGED_CONDITION, SOURCE_STAGED_REASON,
};
use crate::context::Context;
use crate::error::{Error, Result, error_policy_for};
use crate::io::{self, ResolvedRepository};
use crate::jobs::{self, JobLimits, MoverJobInputs, VolumeMountSpec};

mod batch;
mod build;
mod plan;

pub use batch::*;
pub(crate) use build::*;
pub use plan::*;

#[cfg(test)]
mod tests;

/// Steady-state requeue for a **terminal** `Snapshot` (Succeeded/Failed/Discovered
/// and the settled pin/reap tails). These re-reconciles are pure no-ops — every
/// cleanup transition is one-shot-stamped — so the interval is a straight
/// reconcile-QPS lever with no behavior change (issue #249). At 600s a fleet of
/// ~5k terminal CRs pinned ~8 no-op reconciles/second forever; 45 min cuts that by
/// ~4.5×. Kept under an hour so a stuck-but-terminal CR still self-checks a couple
/// of times per hour. Active work (pin spawn, staging, Job polling) keeps its own
/// short requeues — only the terminal steady state is slowed here.
const TERMINAL_SNAPSHOT_STEADY_REQUEUE: Duration = Duration::from_secs(45 * 60);

/// Requeue for a `Snapshot` held on a phase string this build cannot interpret
/// (`SnapshotPhase::Unknown`, i.e. version skew). Deliberately a requeue rather
/// than `await_change()`: nothing about the OBJECT has to change for this to
/// resolve — finishing the operator rollout does — so the reconciler re-checks
/// periodically and re-emits its warning instead of going quiet forever.
const UNKNOWN_PHASE_HOLD_REQUEUE: Duration = Duration::from_secs(10 * 60);

/// Reconcile a `Snapshot`.
///
/// IO is intentionally thin here: the decision logic ([`plan_deletion`],
/// [`effective_deletion_policy`], the job builders in [`crate::jobs`]) is pure
/// and unit-tested; this function wires those decisions to the cluster.
#[tracing::instrument(skip(backup, ctx), fields(kind = "Snapshot", namespace = %backup.namespace().unwrap_or_default(), name = %backup.name_any()))]
pub async fn reconcile(backup: Arc<Snapshot>, ctx: Arc<Context>) -> Result<Action> {
    // A dispatched reconcile is proof the Snapshot reflector completed its initial
    // LIST (the applier gates reconciles on `store.wait_until_ready()`), so the
    // mass-deletion breaker's per-repo pending count is safe to trust from here on.
    // This is the RELIABLE synced signal — see `Context::mark_snapshot_synced`.
    ctx.mark_snapshot_synced();
    let start = std::time::Instant::now();
    let result = reconcile_inner(&backup, &ctx).await;
    ctx.metrics
        .record_reconcile("Snapshot", start.elapsed().as_secs_f64());
    result
}

/// The `policy` label for a Snapshot's completion counter: `spec.policyRef.name`,
/// or `None` for a discovered snapshot (no policyRef) so the label is omitted.
fn backup_policy(backup: &Snapshot) -> Option<&str> {
    backup.spec.policy_ref.as_ref().map(|p| p.name.as_str())
}

/// Whether the Snapshot's kstatus `Stalled` condition is already `True` — i.e. the
/// controller has finalized this terminal failure's conditions. A mover-stamped
/// `Failed` has no such condition (the mover writes only `phase`), which is the seam
/// the TerminalFailed heal + completion count keys on.
fn snapshot_stalled(backup: &Snapshot) -> bool {
    backup.status.as_ref().is_some_and(|s| {
        s.conditions
            .iter()
            .any(|c| c.type_ == crate::consts::STALLED_CONDITION && c.status == "True")
    })
}

/// Whether a Snapshot's failure looks repository-shaped (the backend, not the
/// source, is the likely culprit) — the gate for nudging the repository to
/// re-probe. True when the failing op was the repository connect, or the
/// error class is RepositoryUnavailable whatever the op. A source-level
/// failure (bad PVC path → NotFound on `snapshot create`) must NOT nudge:
/// the probe would succeed anyway, but the nudge churn is pointless (#345).
///
/// `None` (no `status.failure` at all — e.g. a controller-stamped
/// `MoverJobFailed` where the mover never PATCHed a failure block) is
/// fail-safe **true**: with no evidence the failure was source-level, still
/// nudge — the nudge is cheap, rate-limited, and Ready-gated internally.
pub(crate) fn repository_shaped_failure(failure: Option<&FailureBlock>) -> bool {
    let Some(f) = failure else {
        return true;
    };
    // Both comparisons reference the producing enums' stable labels
    // (`kopiur_mover::error::KopiaOp::as_str`, `KopiaErrorClass::as_str`)
    // rather than string literals, so a label rename breaks compilation here
    // instead of silently disabling the gate.
    f.op.as_deref() == Some(kopiur_mover::error::KopiaOp::RepositoryConnect.as_str())
        || f.kopia_error_class == kopiur_kopia::KopiaErrorClass::RepositoryUnavailable.as_str()
}

/// The heal message for a mover-stamped terminal failure. Derived ONLY from
/// `status.failure` fields (op + class — never timestamps or stderr), so
/// repeated reconciles of the same outcome produce a byte-identical message
/// and the `patch_status_if_changed` guard stays a true no-op.
fn mover_failed_message(failure: Option<&FailureBlock>) -> String {
    match failure {
        Some(f) => {
            let class = &f.kopia_error_class;
            match f.op.as_deref() {
                Some(op) => format!(
                    "the backup failed ({op}, {class}): see status.failure and the mover \
                     Job/pod logs"
                ),
                None => format!(
                    "the backup failed ({class}): see status.failure and the mover \
                     Job/pod logs"
                ),
            }
        }
        None => "the backup failed; see status.failure and the mover Job/pod logs".to_string(),
    }
}

/// `Origin::Discovered` steady-state pin: catalog rows, not runs — never spawn a
/// Job. Pins the `Discovered` phase (with kstatus Ready, ADR-0005 §2) and
/// `status.origin` if unset, then stops. Extracted from `reconcile_inner`'s
/// `match origin` (pure refactor of existing behavior — no semantic change) to
/// keep the match legible under the complexity ratchet.
async fn pin_discovered_row(backup: &Snapshot, api: &Api<Snapshot>, name: &str) -> Result<Action> {
    if needs_terminal_pin(
        backup.status.as_ref().and_then(|s| s.phase.as_ref()),
        &SnapshotPhase::Discovered,
    ) {
        let mut status = snapshot_ready_status(
            backup,
            SnapshotPhase::Discovered,
            "Discovered",
            "catalog-materialized snapshot",
        );
        status["origin"] = serde_json::json!("discovered");
        io::patch_status(api, name, status).await?;
    }
    Ok(Action::requeue(TERMINAL_SNAPSHOT_STEADY_REQUEUE))
}

/// `Origin::Adopted` steady-state pin: a row the catalog scan / auto-adoption
/// re-attached to a live `SnapshotPolicy` without the operator itself producing
/// the underlying kopia snapshot. It is retention-visible HISTORY — like
/// `Discovered`, it must never enter the backup-run machinery — but unlike a
/// plain `Discovered` row it IS retention-governed (ADR: adopted rows are
/// managed like any produced backup), so it pins `phase: Succeeded`, not
/// `Discovered`. Mirrors [`pin_discovered_row`]'s idempotent guard: the status
/// patch (phase + terminal kstatus Ready + `status.origin`) only fires when the
/// phase hasn't already converged, so a repeat reconcile is a no-op.
///
/// Belt-and-braces: an adopted row is normally stamped with the
/// snapshot-cleanup finalizer by the mutating webhook at admission (same as
/// every other `Snapshot`), but this arm returns before `reconcile_inner`'s main
/// `ensure_finalizer` call below, so self-heal it here too — a no-op cluster
/// call when the finalizer is already present.
async fn pin_adopted_row(backup: &Snapshot, api: &Api<Snapshot>, name: &str) -> Result<Action> {
    io::ensure_finalizer(api, backup, SNAPSHOT_CLEANUP_FINALIZER).await?;
    // PROVENANCE GATE: pin `Succeeded`/terminal-kstatus ONLY for a
    // CONTROLLER-WRITTEN adopted row — one whose `status.snapshot` was set by
    // `adopt_one`'s create→status-patch flow. A user-applied BARE `origin: adopted`
    // label with no `status.snapshot` resolves `Adopted` (via the label fallback in
    // `resolve_origin`) but must stay PHASE-LESS: a phantom `Succeeded` row would
    // enter `retention_view` (creationTimestamp fallback), claim a GFS bucket, and
    // displace a REAL snapshot into the breaker-exempt retention delete set — and it
    // would set `has_history`, suppressing a recreated policy's on-demand scan. The
    // genuine adopt flow converges within a pass (create → status patch → next
    // reconcile pins), so an interim row is only transiently phase-less and invisible
    // to retention.
    if plan::adopted_row_has_provenance(backup)
        && needs_terminal_pin(
            backup.status.as_ref().and_then(|s| s.phase.as_ref()),
            &SnapshotPhase::Succeeded,
        )
    {
        let mut status = snapshot_ready_status(
            backup,
            SnapshotPhase::Succeeded,
            "Adopted",
            "adopted snapshot: retention-visible history re-attached to a SnapshotPolicy",
        );
        status["origin"] = serde_json::json!("adopted");
        io::patch_status(api, name, status).await?;
    }
    Ok(Action::requeue(TERMINAL_SNAPSHOT_STEADY_REQUEUE))
}

/// `Origin::Replicated` steady-state pin, modeled on [`pin_adopted_row`]: a
/// dest-side copy CR a `SnapshotReplication` run minted for a kopia snapshot it
/// `snapshot migrate`d — catalog history, NEVER a run, so like
/// `Discovered`/`Adopted` it must never enter the backup-run machinery or mint
/// a mover Job. It gets exactly what an adopted row gets: cleanup-finalizer
/// upkeep (its CR deletion cascades to the dest manifest via the normal batched
/// snapdel path) plus an idempotent, PROVENANCE-GATED terminal pin.
///
/// The provenance gate is the same one `pin_adopted_row` uses
/// ([`plan::adopted_row_has_provenance`]: controller/mover-written
/// `status.snapshot`): the replication mover stamps the copy's status (phase
/// `Succeeded` + `snapshot` + `resolved.repository`) in ONE atomic body right
/// after CREATE, so a genuine copy is only transiently phase-less and this pin
/// is a heal for a mover that died between its status patch landing and the
/// phase converging. A user-applied BARE `origin: replicated` label with no
/// `status.snapshot` must stay phase-less — a phantom `Succeeded` row would
/// otherwise claim history it does not have (the same forgery shape
/// `pin_adopted_row` guards against).
///
/// Nothing produces `replicated` rows yet (#368 shared-foundations milestone);
/// this arm exists so the reconciler is total over `Origin` the moment the
/// first copy CR appears.
async fn pin_replicated_row(backup: &Snapshot, api: &Api<Snapshot>, name: &str) -> Result<Action> {
    io::ensure_finalizer(api, backup, SNAPSHOT_CLEANUP_FINALIZER).await?;
    if plan::adopted_row_has_provenance(backup)
        && needs_terminal_pin(
            backup.status.as_ref().and_then(|s| s.phase.as_ref()),
            &SnapshotPhase::Succeeded,
        )
    {
        let mut status = snapshot_ready_status(
            backup,
            SnapshotPhase::Succeeded,
            "Replicated",
            "replicated snapshot: a dest-side copy minted by a SnapshotReplication run",
        );
        status["origin"] = serde_json::json!("replicated");
        io::patch_status(api, name, status).await?;
    }
    Ok(Action::requeue(TERMINAL_SNAPSHOT_STEADY_REQUEUE))
}

async fn reconcile_inner(backup: &Snapshot, ctx: &Context) -> Result<Action> {
    let origin = resolve_origin(backup);
    let policy = effective_deletion_policy(backup.spec.deletion_policy, origin);
    let namespace = backup
        .namespace()
        .ok_or_else(|| Error::Invariant("Snapshot has no namespace".into()))?;
    let name = backup.name_any();
    let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), &namespace);

    if backup.metadata.deletion_timestamp.is_some() {
        // `policy` is already conservative for an unparseable origin
        // (`effective_deletion_policy(_, None)` = forced Retain): the finalizer
        // releases the CR without ever contacting the repository.
        return handle_deletion(backup, ctx, &api, &namespace, &name, policy).await;
    }

    // An origin marker this build cannot parse (a typo, a forged label, or a
    // row written by a NEWER operator during version skew): warn and HOLD —
    // never fall through to the backup-run machinery (the old behavior folded
    // unknown to Manual, which minted a mover Job for a foreign row). Inert on
    // purpose: no finalizer, no phase pin, no Job — just surface it and keep
    // re-checking at the steady cadence in case a newer writer completes it.
    let Some(origin) = origin else {
        tracing::warn!(
            backup = %name, namespace = %namespace,
            label = backup.labels().get(crate::consts::ORIGIN_LABEL).map(String::as_str).unwrap_or(""),
            "unrecognized origin label on Snapshot: this build cannot classify the row, so it \
             will not run, retain-count, or delete it; fix (or remove) the \
             kopiur.home-operations.com/origin label, or upgrade the operator"
        );
        return Ok(Action::requeue(TERMINAL_SNAPSHOT_STEADY_REQUEUE));
    };

    // Exhaustive over `Origin` (ADR §5.5): `Discovered`, `Adopted`, and
    // `Replicated` rows are catalog history, not runs, and must return BEFORE
    // any of the backup-run machinery below (`run_decision`, post-hooks,
    // staged reap, pin jobs) — `Scheduled`/`Manual` are the only origins that
    // ever mint a mover Job, so they fall through to it.
    match origin {
        Origin::Discovered => return pin_discovered_row(backup, &api, &name).await,
        Origin::Adopted => return pin_adopted_row(backup, &api, &name).await,
        Origin::Replicated => return pin_replicated_row(backup, &api, &name).await,
        Origin::Scheduled | Origin::Manual => {}
    }

    // Ensure the snapshot-cleanup finalizer before doing any work that creates a
    // snapshot, so a delete during the run still triggers cleanup.
    if io::ensure_finalizer(&api, backup, SNAPSHOT_CLEANUP_FINALIZER).await? {
        // Requeue so the next pass sees the finalizer.
        return Ok(Action::requeue(Duration::from_secs(1)));
    }

    // One-shot discipline (see [`run_decision`]): a terminal Snapshot must never
    // mint another mover Job — the TTL-reaped Job's deletion event would
    // otherwise re-create it and re-run the backup, forever. This also covers
    // the hook-failure case (ADR §4.8): a hook abort is `Failed`, and without
    // the gate the next reconcile would re-run side-effecting hooks
    // (quiesce/exec) or resurrect the Failed phase to Succeeded — the fix lives
    // in the SnapshotPolicy, and a NEW Snapshot picks it up.
    match run_decision(backup.status.as_ref().and_then(|s| s.phase.as_ref())) {
        RunDecision::Run => {}
        RunDecision::SucceededSteadyState => {
            // Two terminal successes share this arm: `Succeeded` (kopia wrote a
            // manifest this CR owns) and `Unchanged` (kopia deduped, so this CR
            // owns nothing — #351). Everything below is identical for both:
            // afterSnapshot hooks, staged-source teardown, credential reap,
            // projection-pin backfill. They diverge in exactly two places, both
            // marked: the healed phase/reason/message, and pinning — which acts
            // on a manifest an `Unchanged` run does not have.
            let unchanged = backup.status.as_ref().and_then(|s| s.phase.as_ref())
                == Some(&SnapshotPhase::Unchanged);
            // The MOVER stamps `phase: Succeeded`, so the controller's first look
            // at a finished run can already be steady-state — the afterSnapshot
            // hooks (resume/notify) must still run, exactly once. Safe against
            // the re-run hazard this gate exists for: `run_post_hooks_once`
            // self-gates on `status.hooks.postCompletedAt`, so once stamped this
            // is a no-op forever (ADR §4.8).
            if let Some((failure, policy_name)) =
                run_post_hooks_once(backup, ctx, &api, &namespace, &name).await?
            {
                return fail_for_hook(
                    ctx,
                    backup,
                    &api,
                    &namespace,
                    &name,
                    &failure,
                    crate::hooks::HookPhase::After,
                    &policy_name,
                )
                .await;
            }
            // The kstatus conditions come from the controller's transition patch
            // (`finalize_succeeded`) — which the mover's own `phase: Succeeded`
            // stamp can race past (the Job-completion reconcile then already
            // sees Succeeded and lands here, never in the Job branch below).
            // Heal once: patch ONLY phase + conditions (`snapshot_ready_status`
            // carries no `snapshot` key, so the merge preserves the id/identity
            // the mover recorded). Never call `finalize_succeeded` here: its
            // resolve picks the NEWEST manifest for the source path, and
            // healing N sibling Snapshots after the fact would converge them
            // all onto one shared id — corrupting the CR↔manifest binding the
            // deletion finalizer depends on.
            let ready = backup.status.as_ref().is_some_and(|s| {
                s.conditions
                    .iter()
                    .any(|c| c.type_ == crate::consts::READY_CONDITION && c.status == "True")
            });
            if !ready {
                let (phase, reason, message) = if unchanged {
                    (
                        SnapshotPhase::Unchanged,
                        "NoChanges",
                        "no files changed since the previous snapshot, so kopia created no new \
                         snapshot; the previous one remains this source's restore point",
                    )
                } else {
                    (
                        SnapshotPhase::Succeeded,
                        "SnapshotCreated",
                        "the kopia snapshot was created successfully",
                    )
                };
                // Healing with a hard-coded `Succeeded` here would silently
                // overwrite the mover's `Unchanged` on the very next reconcile
                // — and then `finalize_succeeded`/`reconcile_pin` would go
                // looking for a manifest this CR never created.
                io::patch_status(
                    &api,
                    &name,
                    snapshot_ready_status(backup, phase, reason, message),
                )
                .await?;
                // The MOVER stamped `phase: Succeeded` (the common in-cluster path,
                // so `finalize_succeeded` never ran) and we are healing the kstatus
                // (`!ready`). Count the terminal transition here — the symmetric
                // partner to the `finalize_succeeded` count; the two paths are
                // mutually exclusive for a given Snapshot, so this fires once per
                // terminal transition in the common path. A reflector-cache-lagged
                // concurrent reconcile can re-observe `!ready` before the healed
                // status lands, adding a bounded duplicate count — never an
                // under-count.
                ctx.metrics.inc_snapshot_completed(
                    if unchanged { "unchanged" } else { "succeeded" },
                    &namespace,
                    backup_policy(backup),
                );
            }
            // Certain incompleteness signal: the mover recorded source entries kopia
            // EXCLUDED (the ignore-file-errors path — an otherwise-silent partial backup).
            // Flag it once-per-transition. Best-effort; never derails steady-state.
            assess_completed_backup(ctx, &api, &name, backup).await;
            // Staged-source reap is normally done at the Succeeded transition;
            // re-issuing here covers a crash between the phase patch and the
            // cleanup (idempotent, no-op for Direct). BUT the mover stamps
            // `Succeeded` before its pod exits and before the Job controller
            // marks the Job terminal, so this branch can run while the Job is
            // still Active. Tearing the staged PVC + mover pod down now would
            // strand an unschedulable replacement pod (#103) — gate the reap on
            // the Job being terminal (or already gone).
            if backup
                .status
                .as_ref()
                .and_then(|s| s.staged.as_ref())
                .is_some()
            {
                let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), &namespace);
                let job = job_api.get_opt(&name).await?;
                if !staged_teardown_ready(job.as_ref()) {
                    // The owned-Job watch re-triggers us when the Job reaches
                    // Complete; the requeue is a backstop for a missed event.
                    return Ok(Action::requeue(Duration::from_secs(15)));
                }
                io::cleanup_staged_source(
                    &ctx.client,
                    &namespace,
                    &name,
                    backup.spec.source.as_ref().and_then(|s| s.group.as_ref()),
                )
                .await?;
            }
            // The credential copies die with the mover Job, not with this CR (#240).
            // Self-gated by its stamp, so this costs nothing once it has run; if the
            // Job is not terminal yet, the owned-Job watch and the steady-state
            // requeue below both bring us back.
            reap_backup_creds_once(backup, ctx, &api, &namespace, &name).await?;
            // A Snapshot that succeeded under an older operator carries no projection
            // pin, so its finalizer would strand once the recipe is deleted (#255).
            // This branch is the one every terminal Snapshot passes through on startup.
            backfill_projection_pin(backup, ctx, &api, &namespace, &name).await?;
            if unchanged {
                // `spec.pin` acts on a kopia manifest and this run produced
                // none. The only manifest that matches this identity belongs to
                // the PREVIOUS Snapshot CR; pinning it here would both claim
                // another CR's snapshot and — because kopia rewrites a
                // manifest id on pin — invalidate the id that CR recorded.
                return Ok(Action::await_change());
            }
            // §13(c): spec.pin stays live after the mover Job is gone.
            return reconcile_pin(backup, ctx, &api, &namespace, &name, origin).await;
        }
        RunDecision::TerminalFailed => {
            // Resume hooks run even for a FAILED backup (quiesce/resume pairing —
            // a database left locked because the backup failed would turn one
            // incident into two), and the mover may have stamped `Failed` before
            // the controller saw the terminal Job. Self-gated by the stamp; a
            // hook failure here is surfaced on the condition but never masks the
            // primary mover failure.
            if let Some((failure, policy_name)) =
                run_post_hooks_once(backup, ctx, &api, &namespace, &name).await?
            {
                patch_hook_failure(
                    ctx,
                    backup,
                    &api,
                    &name,
                    &failure,
                    crate::hooks::HookPhase::After,
                    &policy_name,
                    false,
                )
                .await?;
            }
            // Heal the kstatus for a terminal failure the controller hasn't
            // finalized yet: the mover stamps only `phase: Failed` (+ the failure
            // block) — never the kstatus conditions — so without this a
            // mover-stamped Failed would lack `Stalled=True` and
            // `kubectl wait --for=condition=Stalled` would never fire. A
            // controller-stamped Failed (MoverJobFailed/MoverPodWedged/preflight)
            // already carries `Stalled=True` (see `snapshot_ready_status`), so this
            // is a no-op there. The `wrote` guard is the seam for counting a
            // mover-stamped (or hook-/refusal-stamped) completion — once per
            // terminal transition in the common path; a reflector-cache-lagged
            // concurrent reconcile can re-observe an unhealed status and add a
            // bounded duplicate count, never an under-count. The controller-stamped
            // paths count at their own write site instead.
            if !snapshot_stalled(backup) {
                // The mover-written failure block (if any) drives both the heal
                // message (byte-stable, derived only from op + class) and the
                // repository-shaped gate on the reverify nudge below.
                let failure = backup.status.as_ref().and_then(|s| s.failure.as_ref());
                let current = serde_json::to_value(&backup.status).ok();
                let wrote = io::patch_status_if_changed(
                    &api,
                    &name,
                    current.as_ref(),
                    snapshot_ready_status(
                        backup,
                        SnapshotPhase::Failed,
                        "SnapshotFailed",
                        &mover_failed_message(failure),
                    ),
                )
                .await?;
                if wrote {
                    ctx.metrics
                        .inc_snapshot_completed("failed", &namespace, backup_policy(backup));
                    // Mirror the controller-stamped branch's reverify nudge: a
                    // mover-stamped Failed is just as likely to mean the backend went
                    // away, but this branch never nudged — so an outage first surfaced
                    // by a mover-stamped failure didn't accelerate the repository's
                    // re-probe (#345). Guarded by `wrote` (once per terminal
                    // transition), same best-effort semantics — and gated on the
                    // failure actually looking repository-shaped (a broken PVC's
                    // NotFound on `snapshot create` proves nothing about the
                    // backend, so nudging would be pointless churn).
                    if repository_shaped_failure(failure) {
                        nudge_repository_reverify(ctx, backup, &name, &namespace).await;
                    }
                }
            }
            // Reap any CSI staging objects the run created. The primary reap runs
            // at the Failed transition itself (the wedge / staging-failure /
            // Job-failed arms), but a transient API error there — or a crash
            // between the phase patch and the cleanup — would otherwise leak the
            // VolumeSnapshot (holding a backend snapshot) until the CR is deleted,
            // because nothing else re-enters cleanup for a Failed Snapshot. This
            // mirrors the Succeeded steady-state reap: gated on the Job being
            // terminal-or-gone (#103 — reaping under an Active Job strands an
            // unschedulable replacement pod), idempotent no-op once the objects
            // are gone.
            if backup
                .status
                .as_ref()
                .and_then(|s| s.staged.as_ref())
                .is_some()
            {
                let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), &namespace);
                let job = job_api.get_opt(&name).await?;
                if !staged_teardown_ready(job.as_ref()) {
                    return Ok(Action::requeue(Duration::from_secs(15)));
                }
                io::cleanup_staged_source(
                    &ctx.client,
                    &namespace,
                    &name,
                    backup.spec.source.as_ref().and_then(|s| s.group.as_ref()),
                )
                .await?;
            }
            // The credential copies die with the mover Job, not with this CR (#240).
            // Unlike the Succeeded arm this one has no steady-state timer, so keep a
            // slow requeue alive until the reap settles rather than leaning on the
            // periodic sweep — which an operator may legitimately have disabled. Once
            // stamped, we go back to `await_change()` and stop polling entirely.
            if !reap_backup_creds_once(backup, ctx, &api, &namespace, &name).await? {
                return Ok(Action::requeue(TERMINAL_SNAPSHOT_STEADY_REQUEUE));
            }
            // `Failed` does NOT imply "no kopia snapshot to delete": a run can create the
            // snapshot, stamp Succeeded + status.snapshot, and only then fail its
            // afterSnapshot hook — `patch_hook_failure` merge-patches phase/conditions and
            // leaves status.snapshot intact. Such a Snapshot still owns real repository
            // data behind a `Delete` finalizer, so it needs the projection pin exactly as
            // much as a Succeeded one. Self-gated, and a no-op when there is no snapshot.
            backfill_projection_pin(backup, ctx, &api, &namespace, &name).await?;
            return Ok(Action::await_change());
        }
        RunDecision::Wait => {
            // Surface loudly rather than parking in silence. `Deleting`/`Discovered`
            // land here as a normal watch-desync wait, but an UNRECOGNIZED phase
            // means version skew (a newer kopiur wrote this object), and a
            // silently-held CR that no diagnostic mentions is exactly the
            // all-green-while-stuck failure of #359. Requeue slowly so the warning
            // repeats and the object self-heals after an operator upgrade.
            if let Some(SnapshotPhase::Unknown(raw)) =
                backup.status.as_ref().and_then(|s| s.phase.as_ref())
            {
                tracing::warn!(
                    namespace = %namespace,
                    snapshot = %name,
                    phase = %raw,
                    "Snapshot is parked on a phase this operator build does not recognize \
                     (a newer kopiur most likely wrote it); holding without acting. Check \
                     for a mixed-version rollout and finish upgrading the operator."
                );
                return Ok(Action::requeue(UNKNOWN_PHASE_HOLD_REQUEUE));
            }
            return Ok(Action::await_change());
        }
    }

    // If the owned mover Job already reached a terminal state, copy phase/stats
    // into status (controller-as-source-of-truth for phase) and stop running.
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), &namespace);
    if let Some(job) = job_api.get_opt(&name).await? {
        match job_terminal_state(&job) {
            Some(true) => {
                // ADR §4.8: afterSnapshot hooks (resume/notify) complete — once —
                // before the terminal Succeeded patch. An aborting failure marks
                // the Snapshot Failed (the kopia snapshot exists, but the hook
                // contract was broken) unless the hook set continueOnFailure.
                if let Some((failure, policy_name)) =
                    run_post_hooks_once(backup, ctx, &api, &namespace, &name).await?
                {
                    return fail_for_hook(
                        ctx,
                        backup,
                        &api,
                        &namespace,
                        &name,
                        &failure,
                        crate::hooks::HookPhase::After,
                        &policy_name,
                    )
                    .await;
                }
                // Which terminal phase the mover already wrote decides what is
                // left to do. `Unchanged` is ALREADY final: kopia deliberately
                // wrote no manifest, so there is nothing to finalize and — the
                // load-bearing part — nothing to resolve. `finalize_succeeded`
                // ends in `resolve_succeeded_snapshot`, which takes the newest
                // manifest matching this identity; for a deduped run that is the
                // PREVIOUS Snapshot CR's manifest. Letting it run here would
                // stamp this CR with a kopia id it does not own, leaving two CRs
                // claiming one manifest and the first prune deleting it out from
                // under the second (#351).
                //
                // Note this was a bare `!= Some(Succeeded)`, which the compiler
                // cannot check — the new phase would have been silently
                // overwritten back to `Succeeded` on the very next reconcile.
                let mover_phase = backup.status.as_ref().and_then(|s| s.phase.as_ref());
                let unchanged = mover_phase == Some(&SnapshotPhase::Unchanged);
                // Exhaustive, not `matches!`: this decides whether to OVERWRITE
                // the phase the mover just wrote, so every phase must state
                // whether it is already a recorded success.
                let already_recorded = match mover_phase {
                    Some(SnapshotPhase::Succeeded | SnapshotPhase::Unchanged) => true,
                    Some(
                        SnapshotPhase::Pending
                        | SnapshotPhase::Running
                        | SnapshotPhase::Failed
                        | SnapshotPhase::Deleting
                        | SnapshotPhase::Discovered,
                    )
                    | None => false,
                    // UNREACHABLE in practice, and deliberately so: `run_decision`
                    // at the top of this reconcile already answered `Wait` for an
                    // unreadable phase and returned (with the warning + hold
                    // requeue), so an `Unknown` Snapshot never gets this far.
                    // `run_decision` is the real guard; this arm exists because
                    // the match is exhaustive and must still state an answer.
                    // `true` (= "leave it alone") is the answer consistent with
                    // that guard — if the early return is ever removed, this
                    // still refuses to overwrite a value the build cannot read.
                    Some(SnapshotPhase::Unknown(_)) => true,
                };
                if !already_recorded {
                    finalize_succeeded(ctx, backup, &api, &name, &namespace).await?;
                }
                // Reap the CSI staging objects (VolumeSnapshot + staged PVC) now the
                // mover is done — frees backend storage promptly (ownerRef GC is the
                // backstop). No-op for Direct (no staged objects recorded).
                if backup
                    .status
                    .as_ref()
                    .and_then(|s| s.staged.as_ref())
                    .is_some()
                {
                    io::cleanup_staged_source(
                        &ctx.client,
                        &namespace,
                        &name,
                        backup.spec.source.as_ref().and_then(|s| s.group.as_ref()),
                    )
                    .await?;
                }
                if unchanged {
                    // Nothing to pin: `spec.pin` acts on a kopia manifest, and
                    // this run produced none. Pinning would have to reach for
                    // the previous CR's manifest — the same ownership confusion
                    // as above, with the added twist that kopia REWRITES a
                    // manifest id on pin, which would invalidate the owner's
                    // recorded id.
                    return Ok(Action::await_change());
                }
                // §13(c): reconcile kopia-side pin state with spec.pin once the
                // snapshot exists. A no-op when already in the desired state.
                return reconcile_pin(backup, ctx, &api, &namespace, &name, origin).await;
            }
            Some(false) => {
                // Resume hooks run even when the backup FAILED — the canonical
                // pairing is quiesce/resume, and a database left locked because
                // the backup failed would turn one incident into two. A hook
                // failure here is surfaced on the condition but cannot mask the
                // primary mover failure.
                if let Some((failure, policy_name)) =
                    run_post_hooks_once(backup, ctx, &api, &namespace, &name).await?
                {
                    patch_hook_failure(
                        ctx,
                        backup,
                        &api,
                        &name,
                        &failure,
                        crate::hooks::HookPhase::After,
                        &policy_name,
                        false,
                    )
                    .await?;
                }
                if backup.status.as_ref().and_then(|s| s.phase.as_ref())
                    != Some(&SnapshotPhase::Failed)
                {
                    io::patch_status(
                        &api,
                        &name,
                        snapshot_ready_status(
                            backup,
                            SnapshotPhase::Failed,
                            "MoverJobFailed",
                            "the backup mover Job failed; see the Job/pod logs",
                        ),
                    )
                    .await?;
                    // Controller-stamped terminal failure (the mover didn't PATCH
                    // Failed): count it once here. This write set `Stalled=True`, so
                    // the follow-up TerminalFailed reconcile won't re-count it.
                    ctx.metrics
                        .inc_snapshot_completed("failed", &namespace, backup_policy(backup));
                    // A failed backup may mean the backend went away: nudge the repository
                    // to re-probe now so the gate engages without waiting for the catalog
                    // refresh. Best-effort — a nudge error must not mask the failure above.
                    // Gated on the failure looking repository-shaped; the mover wrote no
                    // `Failed` phase here but may still have PATCHed a failure block
                    // before the Job gave up, and an absent block is fail-safe true.
                    if repository_shaped_failure(
                        backup.status.as_ref().and_then(|s| s.failure.as_ref()),
                    ) {
                        nudge_repository_reverify(ctx, backup, &name, &namespace).await;
                    }
                }
                // The run is terminal (the Job exhausted its retries) — reap any CSI
                // staging objects. No-op for Direct.
                if backup
                    .status
                    .as_ref()
                    .and_then(|s| s.staged.as_ref())
                    .is_some()
                {
                    io::cleanup_staged_source(
                        &ctx.client,
                        &namespace,
                        &name,
                        backup.spec.source.as_ref().and_then(|s| s.group.as_ref()),
                    )
                    .await?;
                }
                return Ok(Action::requeue(Duration::from_secs(120)));
            }
            None => {
                // Staged-PVC bind watchdog, FIRST: on a WaitForFirstConsumer class the
                // CSI restore/clone only starts when the mover pod schedules, so a slow
                // or hung bind leaves the pod Pending on a Pending claim. While the PVC
                // is provisioning the pod cannot be "wedged" — judging it so at the
                // 300 s pod-startup deadline and reaping the VolumeSnapshot mid-restore
                // was the forgejo/CephFS hourly hard-fail. Bound by the staging budget
                // PINNED at stamp time (the policy may be edited/deleted mid-run), it
                // fails with the same actionable reason as the pre-Job bind gate.
                match staged_pvc_watchdog(backup, ctx, &namespace).await? {
                    StagedPvcWatch::Provisioning => {
                        return Ok(Action::requeue(Duration::from_secs(30)));
                    }
                    StagedPvcWatch::Expired { reason, message } => {
                        io::patch_status(
                            &api,
                            &name,
                            snapshot_ready_status(backup, SnapshotPhase::Failed, reason, &message),
                        )
                        .await?;
                        ctx.metrics.inc_snapshot_completed(
                            "failed",
                            &namespace,
                            backup_policy(backup),
                        );
                        // Stop the kubelet's retry loop, then reap the staged objects
                        // (PVC before VS — the delete order that is safe mid-restore).
                        let _ = job_api.delete(&name, &DeleteParams::background()).await;
                        io::cleanup_staged_source(
                            &ctx.client,
                            &namespace,
                            &name,
                            backup.spec.source.as_ref().and_then(|s| s.group.as_ref()),
                        )
                        .await?;
                        return Ok(Action::requeue(Duration::from_secs(120)));
                    }
                    StagedPvcWatch::Clear => {}
                }
                // A wedged pod (impossible securityContext, missing image, Unschedulable)
                // never reaches a terminal phase, so `backoffLimit` never trips and only
                // the long `activeDeadlineSeconds` backstop would ever stop it — meanwhile
                // the kubelet retries every few seconds, hammering the API. Fail fast once
                // a pod has been wedged past the grace window, with an actionable reason.
                let grace = kopiur_api::common::pod_startup_deadline_seconds(
                    backup.spec.failure_policy.as_ref(),
                );
                if let io::WedgedVerdict::Wedged { reason, message } =
                    io::wedged_pod_verdict(&ctx.client, &namespace, &name, grace).await?
                {
                    io::patch_status(
                        &api,
                        &name,
                        snapshot_ready_status(
                            backup,
                            SnapshotPhase::Failed,
                            "MoverPodWedged",
                            &wedged_pod_message(&reason, &message, grace),
                        ),
                    )
                    .await?;
                    // Controller-stamped terminal failure (set `Stalled=True`): count
                    // once; the follow-up TerminalFailed reconcile won't re-count it.
                    ctx.metrics
                        .inc_snapshot_completed("failed", &namespace, backup_policy(backup));
                    // Delete the wedged Job *and its pod* (Background cascade) so the
                    // kubelet stops retrying immediately — don't wait for TTL/ownerRef
                    // GC. BEST-EFFORT: a delete error must not abort this pass — the
                    // staged-source cleanup below would be skipped and never retried
                    // (the follow-up reconciles route to TerminalFailed).
                    let _ = job_api.delete(&name, &DeleteParams::background()).await;
                    if backup
                        .status
                        .as_ref()
                        .and_then(|s| s.staged.as_ref())
                        .is_some()
                    {
                        io::cleanup_staged_source(
                            &ctx.client,
                            &namespace,
                            &name,
                            backup.spec.source.as_ref().and_then(|s| s.group.as_ref()),
                        )
                        .await?;
                    }
                    return Ok(Action::requeue(Duration::from_secs(120)));
                }
                // Job exists but is still running/starting; mark Running and wait.
                if backup.status.as_ref().and_then(|s| s.phase.as_ref())
                    != Some(&SnapshotPhase::Running)
                {
                    io::patch_status(
                        &api,
                        &name,
                        snapshot_ready_status(
                            backup,
                            SnapshotPhase::Running,
                            "MoverJobRunning",
                            "the backup mover Job is in flight",
                        ),
                    )
                    .await?;
                }
                return Ok(Action::requeue(Duration::from_secs(30)));
            }
        }
    }

    // No Job yet: resolve the recipe and create the mover Job + ConfigMap.
    // A missing `tls.caBundleRef` ConfigMap surfaces as the structural
    // CredentialsAvailable gate (MISSING_CA_BUNDLE_GATE): the retry is
    // transient, but a ConfigMap nobody creates never self-heals, and the park
    // at `Pending` must be visible to doctor (#359).
    let (config, effective_repo, repo) = match resolve_recipe(ctx, backup, &namespace).await {
        Ok(resolved) => resolved,
        Err(Error::MissingCaBundle(msg)) => {
            let existing = backup
                .status
                .as_ref()
                .map(|s| s.conditions.clone())
                .unwrap_or_default();
            let conditions = io::upsert_gate(
                &existing,
                &kopiur_api::gates::MISSING_CA_BUNDLE_GATE,
                &msg,
                backup.meta().generation,
            );
            io::patch_status(
                &api,
                &name,
                serde_json::json!({ "phase": "Pending", "conditions": conditions }),
            )
            .await?;
            io::publish_missing_ca_bundle_event(ctx, backup, &msg).await;
            return Err(Error::MissingCaBundle(msg));
        }
        Err(e) => return Err(e),
    };
    // The EFFECTIVE repository this child runs against (the mint-time pin for
    // a multi-repo fan-out child; the recipe's single ref otherwise) — the one
    // decision `resolve_recipe` already made, reused for every gate below,
    // including the preflight gather (audit M8: the CHILD's repo, never a
    // policy-level guess).
    let repo_ref = &effective_repo;

    // §11: a ReadOnly repository serves restores only — refuse to create a backup
    // Job. Surface a clear condition + Event and stop (not an error: it's a
    // deliberate, terminal-until-spec-change state, so it is counted in the
    // `kopiur_snapshot_refusals` counter rather than reconcile_errors). Restores
    // remain allowed (the Restore reconciler does not gate on mode).
    if !repo.mode.allows_writes() {
        let conds = backup
            .status
            .as_ref()
            .map(|s| s.conditions.clone())
            .unwrap_or_default();
        let conditions = io::upsert_gate(
            &conds,
            &kopiur_api::gates::REPOSITORY_READ_ONLY_GATE,
            &readonly_backup_message(&repo_ref.name),
            backup.meta().generation,
        );
        // Guard the write so the Event + counter + warn fire once per real
        // transition, not on every watch-desync replay of an already-Failed
        // Snapshot (the message is stable, so a repeat is a true no-op).
        let current = serde_json::to_value(&backup.status).ok();
        let wrote = io::patch_status_if_changed(
            &api,
            &name,
            current.as_ref(),
            serde_json::json!({ "phase": "Failed", "conditions": conditions }),
        )
        .await?;
        if wrote {
            ctx.metrics.inc_backup_refused(
                &namespace,
                &name,
                crate::consts::REPOSITORY_READ_ONLY_REASON,
            );
            let _ = ctx
                .recorder
                .publish(
                    &Event {
                        type_: EventType::Warning,
                        reason: crate::consts::REPOSITORY_READ_ONLY_REASON.into(),
                        note: Some(readonly_backup_message(&repo_ref.name)),
                        action: "RefuseBackupReadOnlyRepository".into(),
                        secondary: None,
                    },
                    &io::event_ref(backup),
                )
                .await;
            tracing::warn!(backup = %name, repository = %repo_ref.name, "refusing backup: repository is ReadOnly");
        }
        return Ok(Action::await_change());
    }

    // Don't launch a mover Job against an unreachable repository (`phase != Ready`):
    // the pod would only fail on `kopia repository connect`. Hold the Snapshot in
    // `Pending` and requeue until the repository's own reconcile marks it `Ready`.
    // Same gate Maintenance, `SnapshotPolicy`, and `RepositoryReplication` apply.
    // A cheap single GET — independent of preflight, so it's evaluated FIRST and the
    // repository-not-ready reason is always surfaced before any preflight machinery.
    if !io::repository_ready(&ctx.client, repo_ref, &namespace).await? {
        let current = serde_json::to_value(&backup.status).ok();
        io::patch_status_if_changed(
            &api,
            &name,
            current.as_ref(),
            snapshot_ready_status(
                backup,
                SnapshotPhase::Pending,
                crate::consts::REPOSITORY_NOT_READY_REASON,
                &repository_not_ready_message(&repo_ref.name),
            ),
        )
        .await?;
        return Ok(Action::requeue(Duration::from_secs(15)));
    }

    // Backup preflight (`spec.preflight`, opt-in): the user's CEL preconditions must
    // all hold before the mover Job launches. Evaluated only at FIRST launch
    // (`None`/`Pending`) — a `Running` snapshot whose Job vanished resumes, never
    // re-gated. A failing check holds the Snapshot `Pending` (`PreflightFailed`) and,
    // once `spec.preflight.timeout` elapses from `status.preflightSince`, fails it
    // (bounded so scheduled backups don't pile up `Pending` CRs). `preflight` is the
    // single source of truth here — bound once, so `pf_inputs` is a plain value.
    if let Some(pf) = config
        .spec
        .preflight
        .as_ref()
        .filter(|p| !p.checks.is_empty())
        && should_run_preflight(backup.status.as_ref().and_then(|s| s.phase.as_ref()))
    {
        let now = chrono::Utc::now();
        // Maintenance recency is read from the shared informer; a not-yet-synced
        // (cold/empty) store would fail closed and could spuriously block a
        // maintenance-gated check. Surface the wait so a never-syncing informer
        // (e.g. missing RBAC on `Maintenance`) is diagnosable, not a silent stall.
        if !ctx
            .maintenance_synced
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let current = serde_json::to_value(&backup.status).ok();
            io::patch_status_if_changed(
                &api,
                &name,
                current.as_ref(),
                snapshot_ready_status(
                    backup,
                    SnapshotPhase::Pending,
                    crate::consts::PREFLIGHT_WAITING_REASON,
                    "waiting for the Maintenance cache to sync before evaluating preflight checks",
                ),
            )
            .await?;
            return Ok(Action::requeue(Duration::from_secs(5)));
        }
        // The gather (which clones the Maintenance store for recency) runs only here,
        // so the common no-preflight backup never pays for it.
        let (pf_inputs, _ready) = io::gather_preflight_inputs(
            &ctx.client,
            repo_ref,
            &namespace,
            &ctx.maintenance_store,
            now,
        )
        .await?;
        // First failing check (AND semantics). Distinguish a check that returned
        // `false` (precondition unmet) from one that ERRORED (couldn't be evaluated
        // against live state) — the latter is a config/transient fault, surfaced
        // distinctly so it's diagnosable from the CR, not silently merged into "unmet".
        let failed = pf.checks.iter().find_map(|c| {
            match kopiur_api::eval_preflight_expr(&c.expr, &pf_inputs) {
                Ok(true) => None,
                Ok(false) => Some((c, None)),
                Err(e) => Some((c, Some(e))),
            }
        });
        if let Some((check, eval_err)) = failed {
            // Resolve the timeout: absent ⇒ default; parsed-zero ⇒ indefinite.
            let timeout = kopiur_api::resolve_timeout(
                pf.timeout.as_deref(),
                crate::consts::DEFAULT_PREFLIGHT_TIMEOUT,
            );
            // Anchor the deadline on the FIRST failing reconcile (carried forward),
            // so the timeout budget covers preflight only, not the earlier
            // repository-not-Ready wait. Stamp it ONCE (only when newly failing) so a
            // later reconcile can't push the deadline forward.
            let prior_since = backup
                .status
                .as_ref()
                .and_then(|s| s.preflight_since.clone());
            let newly_failing = prior_since.is_none();
            let since = prior_since.unwrap_or_else(|| now.to_rfc3339());
            let expired = preflight_expired(Some(&since), timeout, now);
            // Deterministic message (no volatile CEL error text → no status churn).
            // An eval error gets its own message and a WARN log carries the detail.
            let msg = match (&eval_err, &check.message) {
                (Some(_), _) => format!(
                    "preflight check {:?} could not be evaluated against the current \
                     repository/maintenance state (see operator logs)",
                    check.name
                ),
                (None, Some(m)) => format!("preflight check {:?} not satisfied: {m}", check.name),
                (None, None) => format!("preflight check {:?} not satisfied", check.name),
            };
            if let Some(e) = &eval_err {
                tracing::warn!(backup = %name, check = %check.name, error = %e, "preflight check could not be evaluated");
            }
            let phase = if expired {
                SnapshotPhase::Failed
            } else {
                SnapshotPhase::Pending
            };
            let mut status =
                snapshot_ready_status(backup, phase, crate::consts::PREFLIGHT_FAILED_REASON, &msg);
            if newly_failing {
                status["preflightSince"] = serde_json::Value::String(since);
            }
            let current = serde_json::to_value(&backup.status).ok();
            let wrote = io::patch_status_if_changed(&api, &name, current.as_ref(), status).await?;
            // Preflight timed out ⇒ terminal Failed. This write set `Stalled=True`, so
            // count once here (guarded by the real transition); TerminalFailed won't
            // re-count it.
            if expired && wrote {
                ctx.metrics
                    .inc_snapshot_completed("failed", &namespace, backup_policy(backup));
            }
            return Ok(if expired {
                Action::await_change()
            } else {
                Action::requeue(Duration::from_secs(30))
            });
        }
        // All checks passed. Clear the one-shot deadline anchor so a *later* failing
        // episode (e.g. this Snapshot is held `Pending` again by a downstream gate
        // like missing credentials, then a check flaps back) starts with a fresh
        // timeout budget instead of the stale anchor from this episode.
        if backup
            .status
            .as_ref()
            .and_then(|s| s.preflight_since.as_ref())
            .is_some()
        {
            io::patch_status(&api, &name, serde_json::json!({ "preflightSince": null })).await?;
        }
    }

    let (mut work_spec, mut source_volume, repo_volume, _) =
        build_backup_run(backup, &config, &repo, &namespace, &name)?;

    // The mover Job runs in THIS (workload) namespace, where the operator SA does
    // not exist. Resolve its run identity here — the user's workload-identity SA
    // (preflighted + bound to the mover role) or the minted mover SA — then verify
    // the credential Secret(s) the mover loads via envFrom are present. Either
    // problem surfaces as a clear `CredentialsAvailable=False` condition + Warning
    // Event and a requeue, instead of launching a Job that hangs (ADR §4.12).
    let mover_identity = match io::ensure_mover_identity(
        &ctx.client,
        &namespace,
        &[&repo.backend],
        ctx.mover_service_account.as_deref(),
        ctx.mover_role_kind.as_str(),
        &ctx.mover_clusterrole,
    )
    .await
    {
        Ok(identity) => identity,
        Err(Error::MissingDependency(msg)) => {
            let existing = backup
                .status
                .as_ref()
                .map(|s| s.conditions.clone())
                .unwrap_or_default();
            let conditions = io::upsert_gate(
                &existing,
                &kopiur_api::gates::MISSING_SERVICE_ACCOUNT_GATE,
                &msg,
                backup.meta().generation,
            );
            io::patch_status(
                &api,
                &name,
                serde_json::json!({ "phase": "Pending", "conditions": conditions }),
            )
            .await?;
            io::publish_missing_sa_event(ctx, backup, &msg).await;
            return Err(Error::MissingDependency(msg));
        }
        Err(e) => return Err(e),
    };

    // Resolve the mover's EFFECTIVE security context once: the explicit
    // `securityContext`, or the one inherited from a workload pod via
    // `inheritSecurityContextFrom`. Both the privileged-mover gate and the Job use it,
    // so an inherited root context is gated exactly like an explicit one.
    // The effective container + pod security contexts — explicit, or both inherited
    // from a workload pod via `inheritSecurityContextFrom`. The backup source PVC (if any)
    // powers the `pvcConsumer` auto-derive mode.
    let source_pvc = source_volume.as_ref().and_then(|v| match &v.source {
        jobs::MountSource::Pvc { claim_name } => Some(claim_name.as_str()),
        jobs::MountSource::Nfs { .. } => None,
    });
    let mover_security = io::resolve_mover_security_contexts(
        &ctx.client,
        &namespace,
        config.spec.mover.as_ref(),
        source_pvc,
        // `inheritSecurityContextFrom.snapshot` is restore-only (admission-rejected
        // on SnapshotPolicy), so a backup never has a recorded source to pass.
        None,
    )
    .await?;
    let (effective_sc, effective_pod_sc) = mover_security.contexts.clone();
    let privileged_mode = config.spec.mover.as_ref().and_then(|m| m.privileged_mode);

    // Field-wise merge the repository's moverDefaults under the recipe's effective
    // contexts/resources/cache: `hardened ⊂ moverDefaults ⊂ recipe` (ADR-0004 §1/§2).
    // Both the privileged-mover gate below and the Job run on the MERGED result, so an
    // elevation introduced by moverDefaults is gated too, and a partial recipe override
    // can only tighten (never drops the hardened drop:[ALL]/seccomp).
    let resolved_mover = kopiur_api::common::resolve_mover(
        repo.mover_defaults.as_ref(),
        effective_sc.as_ref(),
        effective_pod_sc.as_ref(),
        config
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.resources.as_ref()),
        config.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
        // Recipe `mover.ttlSecondsAfterFinished` wins over the repo default (§12).
        config
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.ttl_seconds_after_finished),
    );

    // Record the RESOLVED mover identity on the kopia snapshot itself (the
    // `kopiur-meta` tag) AND on `status.recorded` below — one value feeds both,
    // so the tag and the status can never diverge. This is what lets a restore
    // on a rebuilt cluster (workload not deployed, nothing in etcd) reproduce
    // the identity the data expects.
    let recorded = recorded_meta(
        &resolved_mover,
        &mover_security.outcome,
        config.spec.mover.as_ref(),
    );
    match &mut work_spec.operation {
        Operation::Snapshot(op) => {
            op.tags.insert(
                kopiur_api::KOPIUR_META_TAG.to_string(),
                kopiur_api::encode_meta_tag(&recorded),
            );
        }
        // build_backup_run constructs Operation::Snapshot by construction; every
        // other variant is an invariant breach, enumerated so a new operation
        // cannot compile into silence here.
        Operation::Restore(_)
        | Operation::SnapshotDelete(_)
        | Operation::SnapshotDeleteBatch(_)
        | Operation::BootstrapRepository(_)
        | Operation::Maintenance(_)
        | Operation::SnapshotPin(_)
        | Operation::Verify(_)
        | Operation::Replicate(_)
        | Operation::BrowseSession(_)
        | Operation::SnapshotReplicate(_) => {
            return Err(Error::Invariant(
                "build_backup_run produced a non-Snapshot operation".into(),
            ));
        }
    }

    // Privileged-mover gate (ADR §4.11/§G16, VolSync-parity): an elevated mover
    // (root/privileged/added caps/`privilegedMode`, container- OR pod-level) requires
    // the workload namespace to opt in via the
    // `kopiur.home-operations.com/privileged-movers` annotation — a tenant there could
    // otherwise reuse the minted mover SA at that privilege. Refuse with a clear
    // `MoverPermitted=False` condition + Event otherwise.
    if kopiur_api::common::requires_privilege_resolved(
        Some(&resolved_mover.security_context),
        resolved_mover.pod_security_context.as_ref(),
        privileged_mode,
    ) && !io::namespace_allows_privileged_movers(&ctx.client, &namespace).await?
    {
        let sa = ctx
            .mover_service_account
            .as_deref()
            .unwrap_or(config::DEFAULT_MOVER_NAME);
        let msg =
            io::privileged_mover_message("SnapshotPolicy", &config.name_any(), &namespace, sa);
        let existing = backup
            .status
            .as_ref()
            .map(|s| s.conditions.clone())
            .unwrap_or_default();
        let conditions = io::upsert_gate(
            &existing,
            &kopiur_api::gates::PRIVILEGED_MOVER_GATE,
            &msg,
            backup.meta().generation,
        );
        // Guard the write so the refusal counter + Event fire once per real
        // transition, not on every 30 s transient retry while the namespace
        // opt-in is still absent (the message is stable, so a repeat is a
        // true no-op).
        let current = serde_json::to_value(&backup.status).ok();
        let wrote = io::patch_status_if_changed(
            &api,
            &name,
            current.as_ref(),
            serde_json::json!({ "phase": "Pending", "conditions": conditions }),
        )
        .await?;
        if wrote {
            ctx.metrics.inc_backup_refused(
                &namespace,
                &name,
                PRIVILEGED_MOVER_NOT_PERMITTED_REASON,
            );
            io::publish_warning_event(
                ctx,
                backup,
                PRIVILEGED_MOVER_NOT_PERMITTED_REASON,
                ALLOW_PRIVILEGED_MOVER_ACTION,
                &msg,
            )
            .await;
        }
        // The blocker is the namespace opt-in annotation an admin adds
        // out-of-band. The Namespace watch (`watch::namespace_to_snapshots`)
        // re-enqueues this Snapshot the moment the annotation lands, so the
        // requeue is only a watch-desync backstop — slow structural cadence,
        // not a 30s hot-loop that re-logs the refusal until a human acts.
        return Err(Error::BlockedOnGrant(msg));
    }
    // Permitted: clear any stale `MoverPermitted=False` from a prior reconcile.
    if let Some(conds) = backup.status.as_ref().map(|s| s.conditions.as_slice())
        && conds
            .iter()
            .any(|c| c.type_ == MOVER_PERMITTED_CONDITION && c.status != "True")
    {
        let conditions = io::upsert_condition(
            conds,
            MOVER_PERMITTED_CONDITION,
            true,
            "Permitted",
            "the mover is permitted in this namespace",
            backup.meta().generation,
        );
        io::patch_status(&api, &name, serde_json::json!({ "conditions": conditions })).await?;
    }

    // SecurityContext-compatibility (positive-only, best-effort): confirm `True` only when the
    // RESOLVED mover provably can read the source. Every inherit mode goes through the same
    // honest assessment — `pvcConsumer` used to short-circuit to `True` "by construction",
    // which asserted a match from the fact that this branch ran and was wrong whenever the
    // workload pinned no UID (its identity coming from its image) or the inherited container
    // was a sidecar. `unfiltered_pods` reuses the LIST the pvcConsumer resolver already did, so
    // dropping the short-circuit costs no extra API call. Never writes `False`/Event here —
    // that comes certainly from `assess_completed_backup`.
    if let Some(claim) = source_pvc {
        assess_backup_security_context(
            &namespace,
            backup,
            claim,
            // The mount the assessment must reason about — already decided, so this
            // costs nothing to thread (`readOnly: false` changes what fsGroup means).
            source_volume.as_ref().is_none_or(|v| v.read_only),
            &resolved_mover.security_context,
            resolved_mover.pod_security_context.as_ref(),
            mover_security.unfiltered_pods.as_deref(),
            ctx,
        )
        .await;
    }

    // Report what inheritance actually achieved, when it achieved something other than what
    // the recipe plainly asked for. The compat condition above answers "can the mover read the
    // source"; this answers "did inheriting do what you think it did" — and its silence is the
    // failure mode that produced the false-`True` bug in the first place.
    report_inherit_outcome(
        &namespace,
        backup,
        &mover_security,
        &resolved_mover,
        config.spec.mover.as_ref(),
        ctx,
    )
    .await;

    let owner = io::owner_ref_for(backup, "Snapshot")?;
    // Resolve the credential Secret names the mover loads via envFrom. With
    // `spec.credentialProjection` enabled, the operator copies the repository's
    // Secret(s) into THIS namespace (owned by the Snapshot, GC'd with it) and returns
    // the projected names; otherwise it verifies the user-managed Secret(s) are
    // already present here. Either way a problem surfaces as a clear
    // `CredentialsAvailable=False` condition + Warning Event before we launch a Job
    // that would hang on a missing-Secret envFrom (ADR §4.12).
    let creds = match io::resolve_mover_creds_for(
        &ctx.client,
        &namespace,
        &io::CredsPrefix::snapshot_backup(&name),
        &owner,
        &repo,
        config
            .spec
            .credential_projection
            .as_ref()
            .is_some_and(|p| p.enabled),
        io::repo_kind_str(repo_ref.kind),
        &repo_ref.name,
    )
    .await
    {
        Ok(c) => c,
        Err(Error::MissingDependency(msg)) => {
            let existing = backup
                .status
                .as_ref()
                .map(|s| s.conditions.clone())
                .unwrap_or_default();
            let conditions = io::upsert_gate(
                &existing,
                &kopiur_api::gates::MISSING_CREDENTIALS_GATE,
                &msg,
                backup.meta().generation,
            );
            io::patch_status(
                &api,
                &name,
                serde_json::json!({ "phase": "Pending", "conditions": conditions }),
            )
            .await?;
            io::publish_missing_creds_event(ctx, backup, &msg).await;
            return Err(Error::MissingDependency(msg));
        }
        Err(e) => return Err(e),
    };
    if creds.projected > 0 {
        ctx.metrics
            .inc_secrets_projected(&namespace, creds.projected);
    }
    // Creds are present (or were just projected): clear any stale
    // `CredentialsAvailable=False` from a prior reconcile so a fixed problem stops
    // showing on the object.
    if let Some(conds) = backup.status.as_ref().map(|s| s.conditions.as_slice())
        && conds
            .iter()
            .any(|c| c.type_ == CREDENTIALS_AVAILABLE_CONDITION && c.status != "True")
    {
        let (reason, note) = if creds.projected > 0 {
            (
                CREDENTIALS_PROJECTED_REASON,
                "credential Secret(s) projected into the mover namespace",
            )
        } else {
            (
                "Available",
                "credentials Secret(s) present in the mover namespace",
            )
        };
        let conditions = io::upsert_condition(
            conds,
            CREDENTIALS_AVAILABLE_CONDITION,
            true,
            reason,
            note,
            backup.meta().generation,
        );
        io::patch_status(&api, &name, serde_json::json!({ "conditions": conditions })).await?;
    }
    let creds_secrets = io::plain_creds(creds.names);

    // ADR §4.8: beforeSnapshot hooks (quiesce/flush) run to completion BEFORE the
    // mover Job is created. `status.hooks.preCompletedAt` makes the list run
    // exactly once per Snapshot across requeues and controller restarts — hooks
    // have side effects that must not repeat.
    let hook_spec = config.spec.hooks.clone().unwrap_or_default();
    let pre_done = backup
        .status
        .as_ref()
        .and_then(|s| s.hooks.as_ref())
        .and_then(|h| h.pre_completed_at.as_ref())
        .is_some();
    if !hook_spec.before_snapshot.is_empty() && !pre_done {
        match crate::hooks::run_hooks(
            ctx,
            &namespace,
            &owner,
            &name,
            &hook_spec.before_snapshot,
            crate::hooks::HookPhase::Before,
        )
        .await?
        {
            Some(failure) => {
                return fail_for_hook(
                    ctx,
                    backup,
                    &api,
                    &namespace,
                    &name,
                    &failure,
                    crate::hooks::HookPhase::Before,
                    &config.name_any(),
                )
                .await;
            }
            None => {
                // Stamped BEFORE the Job exists, so a crash between here and the
                // Job apply re-enters with the gate already closed.
                io::patch_status(
                    &api,
                    &name,
                    serde_json::json!({
                        "hooks": { "preCompletedAt": chrono::Utc::now().to_rfc3339() }
                    }),
                )
                .await?;
            }
        }
    }

    // copyMethod: Snapshot/Clone — capture a point-in-time CSI snapshot (or clone) of
    // the source PVC and run the mover against the STAGE, not the live volume (ADR §3.3).
    // Done AFTER beforeSnapshot hooks so a quiesced app yields a consistent capture.
    // `Direct` (and any NFS source) returns NotApplicable and mounts the live source.
    let staged_claim: Option<String> = match io::resolve_staging(
        &ctx.client,
        &ctx.watch_scope,
        &config,
        backup.spec.source.as_ref(),
        &namespace,
        &name,
        &owner,
    )
    .await?
    {
        io::StagingOutcome::NotApplicable => None,
        io::StagingOutcome::Ready(staged) => {
            // Mount the staged PVC in place of the live source — same mount path and
            // kopia source path, so the snapshot's recorded identity is unchanged.
            // `read_only` carries over deliberately: `readOnly: false` (#254) asks the
            // kubelet to apply fsGroup to whatever the mover mounts, and under a staged
            // copyMethod that IS the staged PVC. Dropping it here would silently disable
            // the flag for Snapshot/Clone — the very copyMethods where it is safe.
            if let Some(mount) = source_volume.as_mut() {
                *mount = VolumeMountSpec::pvc(
                    staged.pvc_name.clone(),
                    mount.mount_path.clone(),
                    mount.read_only,
                );
            }
            let existing = backup
                .status
                .as_ref()
                .map(|s| s.conditions.clone())
                .unwrap_or_default();
            let conditions = io::upsert_condition(
                &existing,
                SOURCE_STAGED_CONDITION,
                true,
                SOURCE_STAGED_REASON,
                &format!(
                    "staged source ready ({}): pvc `{}`",
                    staged.copy_method, staged.pvc_name
                ),
                backup.meta().generation,
            );
            io::patch_status(
                &api,
                &name,
                serde_json::json!({
                    "conditions": conditions,
                    "staged": {
                        "copyMethod": staged.copy_method,
                        "volumeSnapshotName": staged.volume_snapshot_name,
                        // Present only under `groupBy: VolumeGroupSnapshot`.
                        // The group has no ownerReferences, so recording it here
                        // is how it stays observable and reapable.
                        "volumeGroupSnapshotName": staged.volume_group_snapshot_name,
                        "pvcName": staged.pvc_name,
                        "ready": true,
                        "storageClassName": staged.storage_class_name,
                        "stagingTimeoutSeconds": staged.staging_timeout_seconds,
                    },
                }),
            )
            .await?;
            Some(staged.pvc_name)
        }
        io::StagingOutcome::Waiting {
            reason,
            message: msg,
        } => {
            // The stage isn't usable yet — a normal, transient wait. Two flavors,
            // named by the carried `reason`: the VolumeSnapshot becoming readyToUse
            // (`WaitingForVolumeSnapshot`) and the staged PVC binding on an
            // Immediate class (`WaitingForStagedPvcBind` — the CSI restore/clone is
            // provisioning). The message may carry the VolumeSnapshot's (possibly
            // transient) `status.error` as diagnostic context; that is NOT a
            // failure — see `StagingOutcome::Failed` for the deadline that is
            // (issue #198).
            let existing = backup
                .status
                .as_ref()
                .map(|s| s.conditions.clone())
                .unwrap_or_default();
            let conditions = io::upsert_condition(
                &existing,
                SOURCE_STAGED_CONDITION,
                false,
                reason,
                &msg,
                backup.meta().generation,
            );
            // if_changed: the wait message is deterministic per VolumeSnapshot
            // (fixed deadline), so steady-state reconciles don't churn status.
            let current = serde_json::to_value(&backup.status).ok();
            io::patch_status_if_changed(
                &api,
                &name,
                current.as_ref(),
                serde_json::json!({ "phase": "Pending", "conditions": conditions }),
            )
            .await?;
            return Err(Error::MissingDependency(msg));
        }
        io::StagingOutcome::Failed { reason, message } => {
            // Staging cannot produce the stage (stack/class missing, source not
            // CSI, or the VolumeSnapshot missed the staging deadline). TERMINAL:
            // this write stamps `Failed` + `Stalled=True`, and the one-shot
            // discipline (`run_decision` → `TerminalFailed` → `await_change`)
            // never re-enters staging — a NEW Snapshot (e.g. the next scheduled
            // run) is how a retry happens. Mirrors the preflight-expired path:
            // patch + specific Warning Event + completed(failed) metric +
            // `Ok(await_change)` — deliberately NOT an `Error::Validation`, whose
            // generic `InvalidSpec` event would tell the user to fix a spec that
            // isn't broken.
            let mut status = snapshot_ready_status_with_condition(
                backup,
                SnapshotPhase::Failed,
                reason,
                &message,
                SOURCE_STAGED_CONDITION,
                false,
            );
            // Stamp the staged block (deterministic names, `ready: false`) even on
            // failure: every cleanup site is gated on `status.staged.is_some()`,
            // and without this stamp a staging-phase failure left an
            // already-created VolumeSnapshot (applied BEFORE the readyToUse
            // deadline is evaluated) holding a backend snapshot until CR deletion.
            let staging_timeout_seconds = kopiur_api::resolve_timeout(
                config
                    .spec
                    .staging
                    .as_ref()
                    .and_then(|s| s.timeout.as_deref()),
                crate::consts::DEFAULT_STAGING_TIMEOUT,
            )
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
            status["staged"] = serde_json::json!({
                "copyMethod": format!("{:?}", config.spec.copy_method),
                // Clone never creates a VolumeSnapshot — don't record a name that
                // could never exist.
                "volumeSnapshotName": (config.spec.copy_method != kopiur_api::CopyMethod::Clone)
                    .then(|| io::volume_snapshot_name(&name)),
                "pvcName": io::staged_pvc_name(&name),
                "ready": false,
                "stagingTimeoutSeconds": staging_timeout_seconds,
            });
            let current = serde_json::to_value(&backup.status).ok();
            let wrote = io::patch_status_if_changed(&api, &name, current.as_ref(), status).await?;
            if wrote {
                let _ = ctx
                    .recorder
                    .publish(
                        &Event {
                            type_: EventType::Warning,
                            reason: reason.to_string(),
                            note: Some(message.clone()),
                            action: FIX_SNAPSHOT_STACK_ACTION.into(),
                            secondary: None,
                        },
                        &io::event_ref(backup),
                    )
                    .await;
                tracing::warn!(backup = %name, reason, "source staging failed: {message}");
                // This write is the real Failed transition (guarded by `wrote`);
                // TerminalFailed won't re-count it.
                ctx.metrics
                    .inc_snapshot_completed("failed", &namespace, backup_policy(backup));
            }
            // Reap whatever staging already created, NOW (idempotent, 404-tolerant,
            // PVC-before-VS — the snapshot-controller's as-source-protection
            // finalizer drains an in-flight restore safely). The stamped `staged`
            // block also lets the terminal-path gates re-issue this on any later
            // reconcile, covering a crash between the patch above and this call.
            io::cleanup_staged_source(
                &ctx.client,
                &namespace,
                &name,
                backup.spec.source.as_ref().and_then(|s| s.group.as_ref()),
            )
            .await?;
            return Ok(Action::await_change());
        }
    };

    let mut labels = run_labels(&config, origin);
    mover_identity.decorate_labels(&mut labels);
    let mut limits = job_limits(backup);
    // moverDefaults.ttlSecondsAfterFinished applies unless the recipe's FailurePolicy
    // already set a TTL (ADR-0005 §12).
    if limits.ttl_seconds_after_finished.is_none() {
        limits.ttl_seconds_after_finished = resolved_mover.ttl_seconds_after_finished;
    }
    // Resolve the cache VOLUME (emptyDir / sized-ephemeral / persistent PVC). A
    // persistent cache PVC is owned by the SnapshotPolicy so a warm cache survives
    // across individual Snapshot runs (ADR §3.1). A pinned child of a MULTI-repo
    // policy gets a per-repository PVC (`kopiur-cache-<policy>-<rslug>-<h6>`,
    // `kopiur_api::expand::cache_pvc_name`): the kopia cache is repository-specific
    // state, so N fan-out children sharing one PVC would poison each other's
    // cache. Single-repo children keep the legacy name byte-identical.
    let cache_pvc = kopiur_api::expand::cache_pvc_name(
        &config.name_any(),
        config.namespace().as_deref().unwrap_or(&namespace),
        kopiur_api::is_multi_repo(&config.spec)
            .then_some(backup.spec.repository.as_ref())
            .flatten(),
    );
    let cache_volume = crate::cache::resolve_cache_volume(
        &ctx.client,
        &namespace,
        io::owner_ref_for(&config, "SnapshotPolicy")?,
        &cache_pvc,
        crate::cache::effective_cache(
            &repo,
            config.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
        )
        .as_ref(),
    )
    .await?;
    // RWO Multi-Attach avoidance: pin the mover to the node the source PVC is
    // attached to, so it co-locates with the app pod already holding the volume.
    // Only the single-`pvc` source needs this — an NFS source is network-mounted, so
    // a Multi-Attach error is impossible. The resolved `sourceColocation` mode
    // (default `Auto`) decides whether/how to pin. RWO multi-attach fix.
    // Co-locate on the EFFECTIVE source claim: the staged PVC when `copyMethod`
    // snapshotted/cloned (a fresh, unheld PVC → resolves to "no pin", so a staged backup
    // schedules freely and is fully decoupled from the source node), else the live PVC
    // (Direct → pin to the node already holding the RWO volume).
    let colo_claim = staged_claim.clone().or_else(|| {
        config
            .spec
            .sources
            .first()
            .and_then(|s| s.pvc.as_ref())
            .map(|p| p.name.clone())
    });
    let (mover_affinity, mover_tolerations) = match colo_claim {
        Some(claim) => {
            // Exhaustive over the outcome (no `_ =>`): the same 404 means
            // different things per claim kind, and only THIS caller knows which
            // kind `claim` is (#382 M5).
            match io::resolve_source_colocation(
                &ctx.client,
                &namespace,
                &claim,
                resolved_mover.source_colocation,
            )
            .await?
            {
                io::ColocationOutcome::Resolved(decision) => {
                    // A previous launch attempt may have parked this Snapshot on
                    // the SourcePvcAvailable gate; flip it True now that the claim
                    // resolved — ONLY when the condition already exists, so the
                    // healthy wire stays byte-identical.
                    clear_source_pvc_gate_if_parked(&api, backup, &name, &namespace, &claim)
                        .await?;
                    io::apply_colocation(
                        decision,
                        resolved_mover.affinity.clone(),
                        resolved_mover.tolerations.clone(),
                    )?
                }
                io::ColocationOutcome::SourcePvcAbsent {
                    namespace: pvc_ns,
                    name: pvc_name,
                } => {
                    // The staged claim (copyMethod Snapshot/Clone) is an
                    // operator-created PVC: its absence is a restage race, a
                    // transient retry — never the user-facing structural gate.
                    if staged_claim.is_some() {
                        return Err(absent_claim_error(true, &pvc_ns, &pvc_name));
                    }
                    // The DIRECT source PVC is gone: park behind the
                    // SourcePvcAvailable gate, fail terminally after the deadline.
                    return handle_missing_source_pvc(
                        ctx, &api, backup, &name, &namespace, &pvc_ns, &pvc_name,
                    )
                    .await;
                }
            }
        }
        None => (
            resolved_mover.affinity.clone(),
            resolved_mover.tolerations.clone(),
        ),
    };
    let inputs = MoverJobInputs {
        name: &name,
        namespace: &namespace,
        owner,
        work_spec: &work_spec,
        image: &ctx.mover_image,
        image_pull_policy: ctx.mover_pull_policy(),
        limits,
        resources: resolved_mover.resources.clone(),
        // The fully-merged contexts (hardened ⊂ moverDefaults ⊂ recipe) — the same
        // values the privileged gate above ran on.
        security_context: resolved_mover.security_context.clone(),
        pod_security_context: resolved_mover.pod_security_context.clone(),
        node_selector: resolved_mover.node_selector.clone(),
        tolerations: mover_tolerations,
        affinity: mover_affinity,
        labels,
        source_volume,
        repo_volume,
        creds_secrets,
        result_configmap: None,
        service_account: mover_identity.service_account.as_deref(),
        passthrough_env: ctx.mover_env_passthrough.clone(),
        extra_env: Vec::new(),
        annotations: Default::default(),
        cache_volume,
        scratch_volume: None,
        readiness_exec: None,
    };
    let job = jobs::build_job(&inputs)?;
    io::apply_mover_objects(&ctx.client, &namespace, &name, None, &job).await?;

    io::patch_status(
        &api,
        &name,
        serde_json::json!({
            "phase": "Running",
            "origin": origin_str(origin),
            // Freeze the run's resolved values (ADR §3.4). The deletion path
            // reads `resolved.repository` so cleanup still works once the
            // recipe is gone — the namespace-deletion cascade usually reaps the
            // SnapshotPolicy (no finalizer) before this Snapshot's finalizer runs.
            "resolved": resolved_run_status(&config, &namespace, &work_spec, repo_ref),
            // The SAME value written into the kopia `kopiur-meta` tag above —
            // single source, so tag and status cannot diverge.
            "recorded": recorded,
        }),
    )
    .await?;
    tracing::info!(backup = %name, "created mover Job for backup");

    Ok(Action::requeue(Duration::from_secs(30)))
}

// --- Missing-source-PVC bounded outcome (#382 M5) ---------------------------
//
// A Snapshot whose DIRECT source PVC was deleted used to map to
// `Error::MissingDependency` (Transient) inside the colocation helper and
// retry every 30-60s forever. It now parks behind the registered
// `SourcePvcAvailable=False` structural gate (stable status bytes, a
// transition-gated Warning Event, the slow 300s cadence) and flips to terminal
// `Failed` once the deadline — anchored on the gate condition's
// `lastTransitionTime`, which `upsert_condition` stamps ONCE and preserves
// while status+message stay unchanged (the `preflightSince` pattern without a
// new status field) — expires. Every decision below is a pure fn over
// condition fixtures.

/// The gate/Event message for a missing DIRECT source PVC. Contains ONLY the
/// PVC identity — no timestamps or attempt counters: volatile status bytes
/// re-trigger the primary watch and hot-loop the reconciler.
pub(super) fn source_pvc_missing_message(pvc_ns: &str, pvc_name: &str) -> String {
    format!(
        "source PVC `{pvc_ns}/{pvc_name}` does not exist, so the backup cannot mount its \
         source; the backup is parked until the missing-source deadline, then failed — \
         recreate the PVC, or update the SnapshotPolicy's spec.sources to name an existing PVC"
    )
}

/// The transient message when the OPERATOR-STAGED claim (copyMethod
/// Snapshot/Clone) vanished mid-launch: a restage race, not a user error —
/// deliberately never "recreate your PVC".
fn staged_claim_missing_message(pvc_ns: &str, claim: &str) -> String {
    format!(
        "operator-staged source PVC `{pvc_ns}/{claim}` vanished before the mover launched \
         (a restage race); staging re-runs automatically on the next reconcile"
    )
}

/// Map an absent colocation claim to its per-caller error (the M5 mapping
/// table): the operator-staged claim is a transient restage race
/// ([`Error::MissingDependency`]); the DIRECT source PVC is the structural
/// parked outcome ([`Error::MissingSourcePvc`]).
pub(super) fn absent_claim_error(staged: bool, pvc_ns: &str, pvc_name: &str) -> Error {
    if staged {
        Error::MissingDependency(staged_claim_missing_message(pvc_ns, pvc_name))
    } else {
        Error::MissingSourcePvc(source_pvc_missing_message(pvc_ns, pvc_name))
    }
}

/// Whether the `SourcePvcAvailable` gate is currently at its blocked polarity
/// (`False`) in `conditions` — the single polarity read the transition-gating
/// and clear decisions below both derive from.
fn source_pvc_gate_is_false(
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> bool {
    conditions
        .iter()
        .any(|c| c.type_ == crate::consts::SOURCE_PVC_AVAILABLE_CONDITION && c.status == "False")
}

/// The deadline ANCHOR: the `SourcePvcAvailable=False` condition's
/// `lastTransitionTime` (RFC 3339), stamped once by the first parking write and
/// preserved by `upsert_condition` while the park persists. `None` when the
/// gate is absent or cleared (`True`) — no anchor, no expiry.
pub(super) fn source_pvc_missing_since(
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> Option<String> {
    conditions
        .iter()
        .find(|c| c.type_ == crate::consts::SOURCE_PVC_AVAILABLE_CONDITION && c.status == "False")
        .map(|c| c.last_transition_time.0.to_string())
}

/// PURE deadline→Failed decision: has the parked-since anchor been missing
/// longer than the configured deadline? `None` deadline (the `0` escape hatch)
/// or no/unparseable anchor → never expired (fail-safe: no spurious terminal
/// `Failed`). Mirrors [`plan::preflight_expired`]'s `>=` boundary.
pub(super) fn source_pvc_deadline_expired(
    since: Option<&str>,
    deadline: Option<std::time::Duration>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let (Some(d), Some(since)) = (
        deadline,
        since.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()),
    ) else {
        return false;
    };
    let elapsed = now - since.with_timezone(&chrono::Utc);
    elapsed >= chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX)
}

/// Transition gate for the Warning Event: publish ONLY when the gate is not
/// already `False` (first sighting, or a recover-then-vanish re-transition) —
/// a steady-state park must not republish every 300s pass.
pub(super) fn should_publish_source_pvc_missing_event(
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> bool {
    !source_pvc_gate_is_false(conditions)
}

/// Whether a successful colocation resolution must flip the gate `True`: ONLY
/// when the condition currently sits at `False`. Absent (the healthy wire — the
/// condition never grows on Snapshots that never hit the gate) or already
/// `True` (cleared) → no write, byte-identical status.
pub(super) fn source_pvc_gate_clear_needed(
    conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
) -> bool {
    source_pvc_gate_is_false(conditions)
}

/// Clear the `SourcePvcAvailable` gate (upsert `True`) after a successful
/// colocation resolution — a no-op unless a previous launch attempt parked this
/// Snapshot on the gate ([`source_pvc_gate_clear_needed`]).
async fn clear_source_pvc_gate_if_parked(
    api: &Api<Snapshot>,
    backup: &Snapshot,
    name: &str,
    pvc_ns: &str,
    pvc_name: &str,
) -> Result<()> {
    let existing = backup
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    if !source_pvc_gate_clear_needed(&existing) {
        return Ok(());
    }
    let conditions = io::upsert_condition(
        &existing,
        crate::consts::SOURCE_PVC_AVAILABLE_CONDITION,
        true,
        crate::consts::SOURCE_PVC_FOUND_REASON,
        &format!("source PVC `{pvc_ns}/{pvc_name}` found; the backup can launch"),
        backup.meta().generation,
    );
    io::patch_status(api, name, serde_json::json!({ "conditions": conditions })).await?;
    Ok(())
}

/// The DIRECT source PVC does not exist: fold the `SourcePvcAvailable=False`
/// gate into the park (`Pending`) or — once [`source_pvc_deadline_expired`] —
/// terminal `Failed` status write, publish the transition-gated Warning Event,
/// and hold on the structural cadence via [`Error::MissingSourcePvc`].
///
/// Terminal safety mirrors the preflight-timeout arm: the `Failed` write
/// carries `Stalled=True` (so `TerminalFailed` never re-counts it), the
/// completion metric increments only on the real transition (`wrote`), and the
/// Failed-no-artifact finalizer path (`execute_delete_snapshot`: no
/// `status.snapshot` → release the finalizer, nothing to delete) already
/// handles the row's eventual pruning.
async fn handle_missing_source_pvc(
    ctx: &Context,
    api: &Api<Snapshot>,
    backup: &Snapshot,
    name: &str,
    namespace: &str,
    pvc_ns: &str,
    pvc_name: &str,
) -> Result<Action> {
    let msg = source_pvc_missing_message(pvc_ns, pvc_name);
    let existing = backup
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    let newly_missing = should_publish_source_pvc_missing_event(&existing);
    // Anchor BEFORE this pass's write: a first sighting has no anchor (this
    // write stamps it), so it can never expire on the same pass it was seen.
    let since = source_pvc_missing_since(&existing);
    let expired = source_pvc_deadline_expired(
        since.as_deref(),
        ctx.source_pvc_deadline,
        chrono::Utc::now(),
    );
    let phase = if expired {
        SnapshotPhase::Failed
    } else {
        SnapshotPhase::Pending
    };
    let status = snapshot_ready_status_with_condition(
        backup,
        phase,
        crate::consts::SOURCE_PVC_MISSING_REASON,
        &msg,
        crate::consts::SOURCE_PVC_AVAILABLE_CONDITION,
        false,
    );
    let current = serde_json::to_value(&backup.status).ok();
    let wrote = io::patch_status_if_changed(api, name, current.as_ref(), status).await?;
    if newly_missing {
        io::publish_warning_event(
            ctx,
            backup,
            crate::consts::SOURCE_PVC_MISSING_REASON,
            crate::consts::RECREATE_SOURCE_PVC_ACTION,
            &msg,
        )
        .await;
    }
    if expired {
        if wrote {
            ctx.metrics
                .inc_snapshot_completed("failed", namespace, backup_policy(backup));
        }
        return Ok(Action::await_change());
    }
    Err(absent_claim_error(false, pvc_ns, pvc_name))
}

/// Best-effort nudge asking this backup's pinned repository to re-verify its
/// backend now (rather than on the next catalog refresh). Called from BOTH
/// terminal-failure stamp sites — controller-stamped `MoverJobFailed` and the
/// mover-stamped `Failed` heal — so an outage accelerates the repository
/// re-probe regardless of who stamped the phase (#345). Best-effort by
/// contract: `request_repository_reverify` is rate-limited (60s per repo) and
/// an error here is logged and swallowed — a nudge failure must never mask the
/// backup failure that triggered it.
async fn nudge_repository_reverify(ctx: &Context, backup: &Snapshot, name: &str, namespace: &str) {
    let Some(repo_ref) = backup
        .status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .and_then(|r| r.repository.as_ref())
    else {
        return;
    };
    if let Err(e) =
        io::request_repository_reverify(&ctx.client, repo_ref, namespace, chrono::Utc::now()).await
    {
        tracing::debug!(backup = %name, error = %e, "repository reverify nudge failed (ignored)");
    }
}

/// Fetch the Snapshot's recipe and run its `afterSnapshot` hooks exactly once,
/// gated by `status.hooks.postCompletedAt`. The stamp is written AFTER the run
/// **whatever the outcome** — the hooks executed, and their side effects
/// (resume, notify) must not repeat on the next requeue. Returns the aborting
/// failure (and the policy name, for the message), if any.
async fn run_post_hooks_once(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
) -> Result<Option<(crate::hooks::HookFailure, String)>> {
    if backup
        .status
        .as_ref()
        .and_then(|s| s.hooks.as_ref())
        .and_then(|h| h.post_completed_at.as_ref())
        .is_some()
    {
        return Ok(None);
    }
    let Some(policy_ref) = backup.spec.policy_ref.as_ref() else {
        // Discovered snapshots have no recipe (and never reach this path).
        return Ok(None);
    };
    let cfg_ns = policy_ref.namespace.as_deref().unwrap_or(namespace);
    let cfg_api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), cfg_ns);
    let Some(config) = cfg_api.get_opt(&policy_ref.name).await? else {
        // The recipe is gone (namespace teardown, manual delete) — nothing to run.
        tracing::warn!(
            backup = %name,
            policy = %policy_ref.name,
            "skipping afterSnapshot hooks: the SnapshotPolicy no longer exists"
        );
        return Ok(None);
    };
    let hooks = config.spec.hooks.clone().unwrap_or_default();
    if hooks.after_snapshot.is_empty() {
        return Ok(None);
    }
    let owner = io::owner_ref_for(backup, "Snapshot")?;
    let failure = crate::hooks::run_hooks(
        ctx,
        namespace,
        &owner,
        name,
        &hooks.after_snapshot,
        crate::hooks::HookPhase::After,
    )
    .await?;
    io::patch_status(
        api,
        name,
        serde_json::json!({
            "hooks": { "postCompletedAt": chrono::Utc::now().to_rfc3339() }
        }),
    )
    .await?;
    Ok(failure.map(|f| (f, config.name_any())))
}

/// Surface an aborting hook failure on the Snapshot: `HooksSucceeded=False` with
/// the actionable message (+ a Warning Event, fired once per real transition via
/// the if-changed guard). `set_failed_phase` additionally moves the phase to
/// `Failed` (the pre-hook / post-hook-after-success paths; the failed-Job path
/// keeps its own `MoverJobFailed` phase write).
#[allow(clippy::too_many_arguments)]
async fn patch_hook_failure(
    ctx: &Context,
    backup: &Snapshot,
    api: &Api<Snapshot>,
    name: &str,
    failure: &crate::hooks::HookFailure,
    phase: crate::hooks::HookPhase,
    policy_name: &str,
    set_failed_phase: bool,
) -> Result<()> {
    let msg = failure.condition_message(phase, policy_name);
    let existing = backup
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    let conditions = io::upsert_condition(
        &existing,
        HOOKS_SUCCEEDED_CONDITION,
        false,
        phase.failed_reason(),
        &msg,
        backup.meta().generation,
    );
    let mut patch = serde_json::json!({ "conditions": conditions });
    if set_failed_phase {
        patch["phase"] = serde_json::json!("Failed");
    }
    let current = serde_json::to_value(&backup.status).ok();
    let wrote = io::patch_status_if_changed(api, name, current.as_ref(), patch).await?;
    if wrote {
        io::publish_warning_event(ctx, backup, phase.failed_reason(), FIX_HOOK_ACTION, &msg).await;
        tracing::warn!(backup = %name, %msg, "aborting hook failure");
    }
    Ok(())
}

/// Terminal hook failure: surface it, mark the Snapshot `Failed`, and stop until
/// the object changes (one-shot semantics — create a new Snapshot once the
/// policy's hook is fixed).
#[allow(clippy::too_many_arguments)]
async fn fail_for_hook(
    ctx: &Context,
    backup: &Snapshot,
    api: &Api<Snapshot>,
    _namespace: &str,
    name: &str,
    failure: &crate::hooks::HookFailure,
    phase: crate::hooks::HookPhase,
    policy_name: &str,
) -> Result<Action> {
    patch_hook_failure(ctx, backup, api, name, failure, phase, policy_name, true).await?;
    Ok(Action::await_change())
}

/// The gathered facts + decided plan for [`handle_deletion`]. Splitting the
/// branchy fact-gathering out of `handle_deletion` keeps that executor under the
/// cognitive-complexity ratchet.
enum DeletionOutcome {
    /// The snapshot reflector store is unset or not yet synced while this CR is
    /// external-destructive: requeue WITHOUT any destructive work and WITHOUT
    /// stamping `DeletionHeld` — a transient startup state, not a breaker trip.
    StoresNotSynced,
    /// A decided plan (boxed — the resolved [`ResolvedRepository`] is large, and
    /// the `StoresNotSynced` variant carries nothing).
    Decided(Box<DeletionDecision>),
}

/// The decided deletion plan plus the executor inputs it needs. `resolved`
/// carries the repository resolution (needed by [`DeletionPlan::DeleteSnapshot`]);
/// `hold` the breaker context (present only when the plan is
/// [`DeletionPlan::HoldSnapshotDeletion`]).
struct DeletionDecision {
    plan: DeletionPlan,
    resolved: Option<Result<(RepositoryRef, ResolvedRepository)>>,
    hold: Option<HoldContext>,
}

/// The context the [`DeletionPlan::HoldSnapshotDeletion`] executor needs to
/// compose its actionable what/why/fix message.
struct HoldContext {
    repo_ref: RepositoryRef,
    pending: usize,
    threshold: u32,
    /// The newest pending `deletionTimestamp` for the repo (RFC3339) — the value
    /// to copy into the `allow-mass-deletion` ack to release the whole wave.
    ack_value: String,
}

/// The store-gated mass-deletion breaker computation for one external-destructive
/// CR.
enum BreakerCompute {
    /// Snapshot store unset or not synced → the caller requeues without work.
    NotSynced,
    /// Computed: the verdict plus the context to compose a Hold message.
    Done {
        state: BreakerState,
        pending: usize,
        threshold: u32,
        ack_value: Option<String>,
    },
}

/// Observed owner state of the deleting `Snapshot`'s `SnapshotSchedule` (M4: the
/// real GET the M2 call site deferred). An operator prune makes the owner
/// IRRELEVANT (`pruned_by` bypasses the cascade guard), so the GET is skipped and
/// `Alive` returned — the guard never consults it, so any non-`GoneOrReplaced`
/// value is correct. Otherwise the ownerRef is resolved via the schedule
/// reflector store (only when populated AND synced — a store miss is a
/// trustworthy 404 only then), falling back to a live `get_opt`, and classified
/// by [`owner_state_from`] (404 / terminating / uid-mismatch ⇒ `GoneOrReplaced`).
async fn resolve_owner_state(
    ctx: &Context,
    backup: &Snapshot,
    namespace: &str,
) -> Result<OwnerState> {
    if pruned_by(backup.annotations()).is_some() {
        return Ok(OwnerState::Alive);
    }
    let Some(owner) = schedule_owner_ref(backup) else {
        return Ok(OwnerState::NoScheduleOwner);
    };
    use std::sync::atomic::Ordering;
    if let Some(store) = ctx.schedule_store.get()
        && ctx.schedule_synced.load(Ordering::Acquire)
    {
        let fetched = store.get(&ObjectRef::<SnapshotSchedule>::new(&owner.name).within(namespace));
        return Ok(owner_state_from(fetched.as_deref(), owner));
    }
    let api: Api<SnapshotSchedule> = Api::namespaced(ctx.client.clone(), namespace);
    let fetched = api.get_opt(&owner.name).await?;
    Ok(owner_state_from(fetched.as_ref(), owner))
}

/// Build the `owner_lookup` for [`pending_members`] from the schedule reflector
/// store. When the store is unset or not synced, EVERY CR resolves to `Alive` —
/// a conservative OVER-count (cascade-guarded CRs that would otherwise be
/// excluded stay counted), so the breaker trips EARLIER, the fail-safe direction.
pub(crate) fn schedule_owner_lookup(ctx: &Context) -> impl Fn(&Snapshot) -> OwnerState {
    use std::sync::atomic::Ordering;
    let synced = ctx.schedule_synced.load(Ordering::Acquire);
    let store = ctx.schedule_store.get().cloned();
    move |backup: &Snapshot| {
        let Some(owner) = schedule_owner_ref(backup) else {
            return OwnerState::NoScheduleOwner;
        };
        match (&store, synced) {
            (Some(store), true) => {
                let ns = backup.namespace().unwrap_or_default();
                let fetched =
                    store.get(&ObjectRef::<SnapshotSchedule>::new(&owner.name).within(&ns));
                owner_state_from(fetched.as_deref(), owner)
            }
            _ => OwnerState::Alive,
        }
    }
}

/// Fold the non-blocking `MassDeletionHeld` condition (ADR-0005 §6; mirrors
/// `IndexBlobHealth` — alert-only, never flips `Ready`) into `existing` from the
/// LIVE pending count for `repo_ref`, read from the shared `Snapshot` reflector
/// store. Returns `existing` UNCHANGED when the store is unset or not synced —
/// skip silently: the deletion path is the source of truth, this repo-side write
/// is an alert-only mirror on the repo's own reconcile cadence. Shared by both
/// repository kinds (the caller supplies its own `RepositoryRef` and raw ack).
///
/// A bad ack is NOT warned about here (the deletion path already publishes
/// `InvalidMassDeletionAck`); it is simply treated as absent for the count.
///
/// `on_namespace_delete` is the repository's cascade policy: the count is
/// evaluated in each member's real `(ns_terminating, ns_policy)` form (resolving
/// terminating state over the pending-delete candidates — only CONFIRMED-terminating
/// namespaces count as terminating here), so this alert-only mirror agrees with the
/// authoritative deletion-path breaker even during a namespace teardown — a
/// `policy-cascade`-stamped child that flips to a destructive external delete under
/// `onNamespaceDelete: Delete` is counted here too. Conversely, a terminating-ns
/// member whose real plan is `OrphanSnapshot` (`onNamespaceDelete: Orphan`) is NOT
/// counted toward the breaker — the mirror softens correctly during Orphan
/// teardowns. The candidate set is bounded by the CRs actually being deleted, so the
/// namespace reads are near-zero in steady state.
pub(crate) async fn repo_mass_deletion_conditions(
    ctx: &Context,
    repo_ref: &RepositoryRef,
    raw_ack: Option<&str>,
    deletion_protection: Option<&kopiur_api::common::DeletionProtectionSpec>,
    on_namespace_delete: NamespaceDeletePolicy,
    existing: &[Condition],
    generation: Option<i64>,
) -> Vec<Condition> {
    use std::sync::atomic::Ordering;
    let present = ctx.snapshot_store.get().is_some();
    let synced = ctx.snapshot_synced.load(Ordering::Acquire);
    let Some(store) = ctx
        .snapshot_store
        .get()
        .filter(|_| breaker_stores_ready(present, synced))
    else {
        return existing.to_vec();
    };
    let now = chrono::Utc::now();
    let (ack, _invalid) = parse_mass_deletion_ack(raw_ack, now);
    let threshold = kopiur_api::consts::effective_mass_deletion_threshold(deletion_protection);
    let state = store.state();
    let key = repo_key(repo_ref);
    // Only CONFIRMED-terminating namespaces feed the count-as-terminating plan
    // evaluation; an unreadable namespace must not flip a retain child into a
    // counted destructive delete (C1).
    let terminating =
        resolve_terminating_namespaces(ctx, &pending_candidate_namespaces(&state, &key)).await;
    let members = pending_members(
        &state,
        &key,
        schedule_owner_lookup(ctx),
        &terminating.confirmed,
        on_namespace_delete,
    );
    let unacked = unacked_breaker_count(&members, ack);
    let newest = newest_pending_deletion(&members).map(|d| d.to_rfc3339());
    let cond = repo_mass_deletion_condition(repo_ref, unacked, threshold, newest.as_deref());
    io::upsert_condition(
        existing,
        crate::consts::MASS_DELETION_HELD_CONDITION,
        cond.held,
        cond.reason,
        &cond.message,
        generation,
    )
}

/// The `allow-mass-deletion` `InvalidMassDeletionAck` Warning, published on the
/// repository CR (the deletion path only holds a [`ResolvedRepository`], so the
/// `ObjectReference` is rebuilt from its stable owner fields — no
/// `resourceVersion`, so the Recorder aggregates repeats).
async fn publish_invalid_ack_event(ctx: &Context, repo: &ResolvedRepository) {
    let o = &repo.owner_ref;
    let regarding = ObjectReference {
        api_version: Some(o.api_version.clone()),
        kind: Some(o.kind.clone()),
        name: Some(o.name.clone()),
        uid: Some(o.uid.clone()),
        namespace: repo.repo_namespace.clone(),
        ..Default::default()
    };
    io::publish_warning_event_on_ref(
        ctx,
        &regarding,
        INVALID_MASS_DELETION_ACK_REASON,
        ACKNOWLEDGE_MASS_DELETION_ACTION,
        &format!(
            "the `{}` annotation on this repository is not a valid RFC3339 timestamp; it is IGNORED \
             (the mass-deletion breaker stays armed). Set it to an RFC3339 instant (e.g. the value \
             the held Snapshots' events surface) to acknowledge a pending wave.",
            crate::consts::ALLOW_MASS_DELETION_ANNOTATION
        ),
    )
    .await;
}

/// Compute the mass-deletion breaker verdict for a deleting external-destructive
/// CR, gated on the snapshot store being SYNCED (fail-safe: a cold cache is never
/// read as "nothing pending"). Counts this repo's unacked external pending
/// deletions ([`pending_members`] + [`unacked_breaker_count`]) and surfaces the
/// newest pending `deletionTimestamp` for the ack command. An unparseable
/// `allow-mass-deletion` ack is ignored (breaker NOT disarmed) and an
/// `InvalidMassDeletionAck` Warning is published on the repository.
async fn resolve_breaker(
    ctx: &Context,
    backup: &Snapshot,
    repo_ref: &RepositoryRef,
    repo: &ResolvedRepository,
) -> BreakerCompute {
    use std::sync::atomic::Ordering;
    let present = ctx.snapshot_store.get().is_some();
    let synced = ctx.snapshot_synced.load(Ordering::Acquire);
    let Some(store) = ctx
        .snapshot_store
        .get()
        .filter(|_| breaker_stores_ready(present, synced))
    else {
        return BreakerCompute::NotSynced;
    };
    let now = chrono::Utc::now();
    let (ack, invalid) = parse_mass_deletion_ack(repo.mass_deletion_ack.as_deref(), now);
    if invalid {
        publish_invalid_ack_event(ctx, repo).await;
    }
    let threshold =
        kopiur_api::consts::effective_mass_deletion_threshold(repo.deletion_protection.as_ref());
    let state = store.state();
    let key = repo_key(repo_ref);
    // Count in the REAL `(ns_terminating, ns_policy)` form so an external
    // destructive delete that only arises under an `onNamespaceDelete: Delete`
    // namespace teardown (a `policy-cascade`-stamped child) is counted — and thus
    // can HOLD the wave — exactly like an unstamped external child. Only
    // CONFIRMED-terminating namespaces count as terminating; an unreadable one
    // stays retain-form for plan/count (C1), matching the self-CR path's
    // `unwrap_or(false)`.
    let terminating =
        resolve_terminating_namespaces(ctx, &pending_candidate_namespaces(&state, &key)).await;
    let members = pending_members(
        &state,
        &key,
        schedule_owner_lookup(ctx),
        &terminating.confirmed,
        repo.on_namespace_delete,
    );
    let pending = unacked_breaker_count(&members, ack);
    let ack_value = newest_pending_deletion(&members).map(|d| d.to_rfc3339());
    let deletion_ts = backup
        .metadata
        .deletion_timestamp
        .as_ref()
        .and_then(|t| chrono::DateTime::from_timestamp(t.0.as_second(), 0))
        .unwrap_or(now);
    let state = breaker_state(pending, threshold, deletion_ts, ack);
    BreakerCompute::Done {
        state,
        pending,
        threshold,
        ack_value,
    }
}

/// Assemble [`DeletionFacts`] and decide the [`DeletionPlan`] for
/// [`handle_deletion`] with REAL facts (M4): the owning schedule's live state
/// ([`resolve_owner_state`]) and this repository's mass-deletion breaker verdict
/// ([`resolve_breaker`]). Kept in its own fn so `handle_deletion` stays a thin
/// executor under the complexity ratchet.
async fn gather_deletion_facts(
    ctx: &Context,
    backup: &Snapshot,
    namespace: &str,
    policy: DeletionPolicy,
    ns_terminating: bool,
) -> Result<DeletionOutcome> {
    let owner = resolve_owner_state(ctx, backup, namespace).await?;
    let cascade = effective_on_schedule_delete(backup.spec.on_schedule_delete);
    let annotations = backup.annotations();
    // Build `DeletionFacts` for a given `(breaker, ns_terminating, ns_policy)` —
    // the other four fields are fixed for this CR (the `annotations` reference is
    // `Copy`, so this closure is callable repeatedly).
    let mk =
        |breaker: BreakerState, ns_t: bool, ns_p: Option<NamespaceDeletePolicy>| DeletionFacts {
            policy,
            annotations,
            owner,
            cascade,
            ns_terminating: ns_t,
            ns_policy: ns_p,
            breaker,
        };

    // Would a per-CR delete run in the live-namespace form? Gates resolving the
    // repository (needed for the ns policy, to place/build the Job, and the
    // breaker repo_key). The repository is ALSO always resolved while the
    // namespace terminates (below), so `ns_policy` is available whenever the real
    // `(ns_terminating, ns_policy)` form needs it.
    let would_delete = matches!(
        plan_deletion(mk(BreakerState::Allowed, false, None)),
        DeletionPlan::DeleteSnapshot
    );

    let resolved = if ns_terminating || would_delete {
        Some(resolve_repo_for_deletion(ctx, backup, namespace).await)
    } else {
        None
    };
    let ns_policy = resolved
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|(_, repo)| repo.on_namespace_delete);

    // Does the breaker apply to THIS deletion (external destructive Delete)?
    // Computed in the REAL `(ns_terminating, ns_policy)` form — NOT the stable
    // `false` form — so an external destructive delete that only arises under a
    // namespace teardown with `onNamespaceDelete: Delete` (e.g. a
    // `policy-cascade`-stamped child whose namespace a human deleted with the
    // explicit opt-in) counts toward / is held by the breaker exactly like an
    // unstamped external child. In the live-namespace case `ns_terminating` is
    // false and `plan_deletion` ignores `ns_policy`, so this equals the old form.
    let breaker_applies =
        counts_toward_breaker(mk(BreakerState::Allowed, ns_terminating, ns_policy));

    // Breaker: only for an external-destructive CR, and only once the repository
    // resolved (its `repo_key` scopes the pending count). A repo that can no
    // longer resolve leaves the breaker `Allowed` — the DeleteSnapshot executor
    // requeues on the resolve error, so nothing is deleted (fail-safe).
    let mut breaker = BreakerState::Allowed;
    let mut breaker_ctx: Option<(usize, u32, Option<String>)> = None;
    if breaker_applies
        && let Some((repo_ref, repo)) = resolved.as_ref().and_then(|r| r.as_ref().ok())
    {
        match resolve_breaker(ctx, backup, repo_ref, repo).await {
            BreakerCompute::NotSynced => return Ok(DeletionOutcome::StoresNotSynced),
            BreakerCompute::Done {
                state,
                pending,
                threshold,
                ack_value,
            } => {
                breaker = state;
                breaker_ctx = Some((pending, threshold, ack_value));
            }
        }
    }

    let plan = plan_deletion(mk(breaker, ns_terminating, ns_policy));
    // A Hold plan carries the breaker context so its executor can compose the
    // actionable message (`Hold ⟹ breaker_applies ⟹ resolved Ok ⟹ ctx Some`).
    let hold = if plan == DeletionPlan::HoldSnapshotDeletion {
        resolved
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .zip(breaker_ctx)
            .map(
                |((repo_ref, _), (pending, threshold, ack_value))| HoldContext {
                    repo_ref: repo_ref.clone(),
                    pending,
                    threshold,
                    ack_value: ack_value.unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
                },
            )
    } else {
        None
    };
    Ok(DeletionOutcome::Decided(Box::new(DeletionDecision {
        plan,
        resolved,
        hold,
    })))
}

/// Execute the deletion plan (the tested [`plan_deletion`] decision, assembled
/// by [`gather_deletion_facts`]) against the cluster, then remove the finalizer
/// when cleanup completes.
async fn handle_deletion(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
    policy: DeletionPolicy,
) -> Result<Action> {
    // Nothing to clean up if our finalizer isn't present.
    if !backup
        .finalizers()
        .iter()
        .any(|f| f == SNAPSHOT_CLEANUP_FINALIZER)
    {
        return Ok(Action::await_change());
    }

    // Reap any CSI staging objects (with the Retain→Delete PV patch) before the kopia
    // snapshot-deletion plan runs. OwnerRef GC removes the staged PVC/VolumeSnapshot when
    // the CR is deleted, but it can't flip a Retain PV's reclaim policy — so do it here.
    if backup
        .status
        .as_ref()
        .and_then(|s| s.staged.as_ref())
        .is_some()
    {
        io::cleanup_staged_source(
            &ctx.client,
            namespace,
            name,
            backup.spec.source.as_ref().and_then(|s| s.group.as_ref()),
        )
        .await?;
    }

    // Namespace-deletion cascade (ADR-0005 §5): if the owning namespace is being torn
    // down, the repository's `onNamespaceDelete` decides. Default `Orphan` keeps
    // off-site history (a `kubectl delete ns` must not be a data-loss event); only an
    // explicit `Delete` cascades to the per-Snapshot plan. On a transient read error
    // fall back to the per-Snapshot plan (`unwrap_or(false)` = NOT terminating): a
    // single delete still works, and the namespace-cascade case re-evaluates on the
    // next pass once the read succeeds. This is the SAME direction the peer-driven
    // batch path takes for an unreadable namespace ([`resolve_terminating_namespaces`]
    // routes `Err` into `unreadable`, which never counts as terminating for
    // plan/count) — so an unreadable namespace never turns a Retain into a delete on
    // either path.
    let ns_terminating = io::namespace_is_terminating(&ctx.client, namespace)
        .await
        .unwrap_or(false);

    let decision = match gather_deletion_facts(ctx, backup, namespace, policy, ns_terminating)
        .await?
    {
        DeletionOutcome::StoresNotSynced => {
            tracing::info!(backup = %name, "snapshot store not synced yet; deferring deletion (no destructive work)");
            return Ok(Action::requeue(Duration::from_secs(15)));
        }
        DeletionOutcome::Decided(d) => *d,
    };
    tracing::info!(plan = ?decision.plan, backup = %name, ns_terminating, "executing backup deletion plan");

    execute_deletion_plan(decision, backup, ctx, api, namespace, name, ns_terminating).await
}

/// Dispatch the tested [`plan_deletion`] decision to its executor. Extracted
/// from [`handle_deletion`] to keep that function under the
/// cognitive-complexity ratchet — every arm here is pure dispatch (no new
/// decision logic; the decision itself is `plan_deletion`'s job).
async fn execute_deletion_plan(
    decision: DeletionDecision,
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
    ns_terminating: bool,
) -> Result<Action> {
    let DeletionDecision {
        plan,
        resolved,
        hold,
    } = decision;
    match plan {
        DeletionPlan::DeleteSnapshot => {
            execute_delete_snapshot(backup, ctx, api, namespace, name, ns_terminating, resolved)
                .await
        }
        DeletionPlan::RetainSnapshot => {
            io::remove_finalizer(api, backup, SNAPSHOT_CLEANUP_FINALIZER).await?;
            ctx.metrics
                .inc_snapshot_deletion(namespace, SnapshotDeletionOutcome::Retained);
            Ok(Action::await_change())
        }
        DeletionPlan::OrphanSnapshot => {
            orphan_snapshot(
                backup,
                ctx,
                api,
                namespace,
                name,
                &format!(
                    "snapshot for backup {name} orphaned (policy/escape-hatch); finalizer removed \
                     without contacting the repository"
                ),
            )
            .await
        }
        DeletionPlan::RetainSnapshotOnScheduleDelete => {
            retain_on_schedule_delete(ctx, api, backup, namespace, name).await
        }
        DeletionPlan::RetainSnapshotOnPolicyDelete => {
            retain_on_policy_delete(ctx, api, backup, namespace, name).await
        }
        DeletionPlan::HoldSnapshotDeletion => hold_deletion(ctx, api, backup, name, hold).await,
    }
}

/// The [`DeletionPlan::DeleteSnapshot`] executor: no recorded kopia snapshot →
/// just release the finalizer; otherwise resolve the repository and hand off to
/// the per-repository BATCH delete dispatcher ([`drive_batch_deletion`]), which
/// coalesces many members into one mover Job (or orphans, when no surviving
/// namespace can host it). Extracted from `handle_deletion` to keep that
/// dispatcher under the complexity ratchet.
async fn execute_delete_snapshot(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
    ns_terminating: bool,
    resolved: Option<Result<(RepositoryRef, ResolvedRepository)>>,
) -> Result<Action> {
    let Some(id) = backup
        .status
        .as_ref()
        .and_then(|s| s.snapshot.as_ref())
        .map(|s| s.kopia_snapshot_id.clone())
    else {
        // No snapshot was ever recorded: nothing to delete in the repo.
        io::remove_finalizer(api, backup, SNAPSHOT_CLEANUP_FINALIZER).await?;
        return Ok(Action::await_change());
    };
    // A repo-resolution blocker on the DELETION path (the CA ConfigMap reaped
    // by namespace teardown, a vanished credential dependency) wedges the
    // finalizer, so it must carry the stuck-finalizer escape hatch (#255).
    let (repo_ref, repo) = match resolved {
        Some(r) => r.map_err(|e| hint_deletion_blocker(e, namespace, name))?,
        // Unreachable by construction (plan=Delete implies the resolution ran);
        // resolve again rather than panic.
        None => resolve_repo_for_deletion(ctx, backup, namespace)
            .await
            .map_err(|e| hint_deletion_blocker(e, namespace, name))?,
    };
    drive_batch_deletion(
        backup,
        ctx,
        api,
        namespace,
        name,
        &id,
        ns_terminating,
        &repo_ref,
        &repo,
    )
    .await
}

/// Executor for [`DeletionPlan::RetainSnapshotOnScheduleDelete`]: the cascade
/// guard fired (owning `SnapshotSchedule` gone/replaced, `onScheduleDelete:
/// Retain`, effective policy was `Delete`). Release the finalizer WITHOUT
/// contacting the repository (same as `RetainSnapshot`), bump the
/// cascade-retained counter (the SINGLE increment point for both counters), and
/// emit ONE Warning event saying what happened, why, and how to opt into
/// cascading deletes.
async fn retain_on_schedule_delete(
    ctx: &Context,
    api: &Api<Snapshot>,
    backup: &Snapshot,
    namespace: &str,
    name: &str,
) -> Result<Action> {
    ctx.metrics.inc_snapshot_cascade_retained(namespace);
    io::publish_warning_event(
        ctx,
        backup,
        SNAPSHOT_RETAINED_ON_SCHEDULE_DELETE_REASON,
        ENABLE_SCHEDULE_CASCADE_ACTION,
        &schedule_cascade_retained_message(namespace, name),
    )
    .await;
    io::remove_finalizer(api, backup, SNAPSHOT_CLEANUP_FINALIZER).await?;
    Ok(Action::await_change())
}

/// Executor for [`DeletionPlan::RetainSnapshotOnPolicyDelete`]: this Snapshot
/// carries a `policy-cascade` prune stamp (its owning `SnapshotPolicy` was
/// deleted under `onPolicyDelete: Retain`) and its own effective
/// `deletionPolicy` is `Delete`. Release the finalizer WITHOUT contacting the
/// repository (same as `RetainSnapshot`), bump the policy-cascade-retained
/// counter (the SINGLE increment point for both counters), and emit ONE
/// Warning event saying what happened, why, and how to opt into cascading
/// deletes. Mirrors [`retain_on_schedule_delete`] exactly in shape.
async fn retain_on_policy_delete(
    ctx: &Context,
    api: &Api<Snapshot>,
    backup: &Snapshot,
    namespace: &str,
    name: &str,
) -> Result<Action> {
    ctx.metrics.inc_snapshot_policy_cascade_retained(namespace);
    let snapshot_recorded = backup
        .status
        .as_ref()
        .and_then(|s| s.snapshot.as_ref())
        .is_some();
    io::publish_warning_event(
        ctx,
        backup,
        SNAPSHOT_RETAINED_ON_POLICY_DELETE_REASON,
        ENABLE_POLICY_CASCADE_ACTION,
        &policy_cascade_retained_message(namespace, name, snapshot_recorded),
    )
    .await;
    io::remove_finalizer(api, backup, SNAPSHOT_CLEANUP_FINALIZER).await?;
    Ok(Action::await_change())
}

/// Executor for [`DeletionPlan::HoldSnapshotDeletion`]: the mass-deletion breaker
/// tripped. Do NO delete work and KEEP the finalizer. One status patch combines
/// `phase: Deleting` with an upserted `DeletionHeld=True` condition (sourced LIVE
/// to avoid clobbering a concurrent condition writer, written via
/// `patch_status_if_changed` for hot-loop hygiene). The message carries the
/// pending count vs. threshold, the repository, the EXACT ack command, and the
/// per-CR escape hatch. The Warning event fires ONLY on the transition into held.
/// Requeue on the long `Held` cadence; the repo ack re-enqueues this CR to drain.
async fn hold_deletion(
    ctx: &Context,
    api: &Api<Snapshot>,
    backup: &Snapshot,
    name: &str,
    hold: Option<HoldContext>,
) -> Result<Action> {
    // Source conditions LIVE — the deletion path is a second condition writer, so
    // building from the start-of-reconcile copy could clobber a concurrent write.
    let Some(live) = io::live_conditions_source(api, name, backup).await else {
        // 404: the CR is already gone — nothing to hold.
        return Ok(Action::await_change());
    };
    let existing = live
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    let message = match &hold {
        Some(h) => mass_deletion_hold_message(&h.repo_ref, h.pending, h.threshold, &h.ack_value),
        // Defensive: a Hold plan always carries context (see `gather_deletion_facts`).
        None => "this snapshot's deletion is held by the mass-deletion breaker; acknowledge the \
                 pending wave on the repository via the `allow-mass-deletion` annotation, or set \
                 the per-Snapshot `skip-snapshot-cleanup` annotation to release it without deleting."
            .to_string(),
    };
    // Transition-only event: fire only when the condition was not already True.
    let emit = should_emit_held_event(&existing);
    let conditions = io::upsert_gate(
        &existing,
        &kopiur_api::gates::DELETION_HELD_GATE,
        &message,
        backup.meta().generation,
    );
    let current = serde_json::to_value(&live.status).ok();
    io::patch_status_if_changed(
        api,
        name,
        current.as_ref(),
        serde_json::json!({ "phase": "Deleting", "conditions": conditions }),
    )
    .await?;
    if emit {
        io::publish_warning_event(
            ctx,
            backup,
            SNAPSHOT_DELETION_HELD_REASON,
            ACKNOWLEDGE_MASS_DELETION_ACTION,
            &message,
        )
        .await;
    }
    Ok(Action::requeue(deletion_requeue(DeletionRequeue::Held)))
}

/// If the `Snapshot` currently carries `DeletionHeld=True`, return the conditions
/// array with it flipped to `False`/`Acknowledged` — to fold into the SAME status
/// patch that moves a previously-held deletion forward. `None` when it was never
/// held, so the common (never-held) delete path adds no condition churn.
fn cleared_held_conditions(backup: &Snapshot) -> Option<Vec<Condition>> {
    let existing = backup
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    let held = existing
        .iter()
        .any(|c| c.type_ == DELETION_HELD_CONDITION && c.status == "True");
    held.then(|| {
        io::upsert_condition(
            &existing,
            DELETION_HELD_CONDITION,
            false,
            MASS_DELETION_ACKNOWLEDGED_REASON,
            "the mass-deletion wave was acknowledged; deletion is proceeding",
            backup.meta().generation,
        )
    })
}

/// Release the finalizer WITHOUT contacting the repository: record the orphan
/// metric, emit a `SnapshotOrphaned` event carrying `note` (why, and how to clean
/// up manually if unwanted), and remove the finalizer.
async fn orphan_snapshot(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
    note: &str,
) -> Result<Action> {
    tracing::info!(backup = %name, note, "orphaning snapshot; releasing finalizer");
    ctx.metrics.inc_orphaned_snapshot(namespace);
    // Also record it on the unified deletion-outcome counter (single increment
    // point per outcome); the existing orphan gauge keeps its own site above.
    ctx.metrics
        .inc_snapshot_deletion(namespace, SnapshotDeletionOutcome::Orphaned);
    let _ = ctx
        .recorder
        .publish(
            &Event {
                type_: EventType::Normal,
                reason: "SnapshotOrphaned".into(),
                note: Some(note.to_string()),
                action: "Orphan".into(),
                secondary: None,
            },
            &io::event_ref(backup),
        )
        .await;
    io::remove_finalizer(api, backup, SNAPSHOT_CLEANUP_FINALIZER).await?;
    Ok(Action::await_change())
}

/// Resolve the repository the deletion path must talk to WITHOUT requiring the
/// recipe to still exist: prefer the ref pinned into `status.resolved.repository`
/// at run time (ADR §3.4), falling back to the live recipe for a `Snapshot` that
/// never ran. The namespace-deletion cascade depends on this — resolving via the
/// recipe alone silently degraded an opted-in `onNamespaceDelete: Delete` to an
/// orphan whenever the namespace reaper got to the `SnapshotPolicy` first.
async fn resolve_repo_for_deletion(
    ctx: &Context,
    backup: &Snapshot,
    namespace: &str,
) -> Result<(RepositoryRef, ResolvedRepository)> {
    if let Some(pinned) = backup
        .status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .and_then(|r| r.repository.as_ref())
    {
        let repo = io::resolve_repository_ref(
            &ctx.client,
            pinned,
            namespace,
            ctx.operator_namespace.as_deref(),
        )
        .await?;
        return Ok((pinned.clone(), repo));
    }
    let (config, effective_repo, repo) = resolve_recipe(ctx, backup, namespace).await?;
    let config_ns = config.namespace().unwrap_or_else(|| namespace.to_string());
    Ok((pinned_repository_ref(&effective_repo, &config_ns), repo))
}

/// Append the deletion-path escape hatch to a credentials error message.
///
/// `resolve_mover_creds`'s messages are shared by every mover, so they cannot name a
/// remedy that exists only for a `Snapshot`'s finalizer. Here they can — and must: on
/// this path a credential error does not merely fail a run, it blocks the CR from being
/// deleted at all, and the user is owed the way out (#255).
///
/// This covers the residue the pin deliberately does not: `credentialProjection.allowed`
/// is resolved from the LIVE repository, so an owner who revokes it — or deletes the
/// `ClusterRepository` outright — still denies projection, correctly. Honoring a revoked
/// consent on the strength of a stale pin would be worse than a stuck finalizer; naming
/// the escape hatch is the right answer instead.
fn stuck_finalizer_hint(msg: &str, namespace: &str, name: &str) -> String {
    format!(
        "{msg} Until this resolves, Snapshot `{namespace}/{name}` stays terminating: \
         `deletionPolicy: Delete` holds the `{SNAPSHOT_CLEANUP_FINALIZER}` finalizer until the \
         kopia snapshot is deleted. To release the CR WITHOUT deleting the kopia snapshot — it \
         stays in the repository and the catalog can rediscover it — annotate the Snapshot \
         `{SKIP_SNAPSHOT_CLEANUP_ANNOTATION}: \"true\"`."
    )
}

/// Enrich a deletion-path blocker with the stuck-finalizer escape hatch
/// ([`stuck_finalizer_hint`]), preserving the error's variant (and therefore
/// its retry class).
///
/// Two dependency shapes can be deleted out from under a terminating
/// `Snapshot` and silently wedge its finalizer: the credential Secret
/// ([`Error::MissingDependency`], #255) and — since `tls.caBundleRef` — the CA
/// ConfigMap ([`Error::MissingCaBundle`]), which namespace teardown reaps just
/// like the Secret. Both must carry the way out; anything else passes through
/// untouched.
pub(super) fn hint_deletion_blocker(e: Error, namespace: &str, name: &str) -> Error {
    match e {
        Error::MissingDependency(m) => {
            Error::MissingDependency(stuck_finalizer_hint(&m, namespace, name))
        }
        Error::MissingCaBundle(m) => {
            Error::MissingCaBundle(stuck_finalizer_hint(&m, namespace, name))
        }
        other => other,
    }
}

/// The per-repository BATCH delete dispatcher (mass-deletion protection), run by
/// [`execute_delete_snapshot`] where the retired per-CR `{name}-delete` Job used
/// to be created. One mover Job (`SnapshotDeleteBatch`, M1) deletes MANY members'
/// kopia manifests over a single connect, gated by the pure decision layer
/// ([`super::batch`]), CREATEd (never SSA-applied), and reaped explicitly.
///
/// Order (brief §1): honor any LEGACY per-CR delete Job (upgrade shim, §0);
/// resolve placement (§1, orphan when nothing survivable can host the Job); gate
/// on the snapshot store being synced (operator PRUNES flow here withOUT the
/// breaker's own store gate, so the dispatcher needs its own fail-safe); LIST
/// this repository's batch Jobs ONCE (§2), reap the drained/aged terminal ones,
/// and classify THIS CR against that same list; if it is not yet a member, decide
/// whether to fire a fresh wave (§3).
#[allow(clippy::too_many_arguments)]
async fn drive_batch_deletion(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
    snapshot_id: &str,
    ns_terminating: bool,
    repo_ref: &RepositoryRef,
    repo: &ResolvedRepository,
) -> Result<Action> {
    // §0 upgrade shim (runs BEFORE placement so a legacy Job is honored even when
    // the batch placement would orphan): a pre-batch `{name}-delete` Job still
    // owns this deletion.
    if let Some(action) =
        adopt_legacy_delete_job(backup, ctx, api, namespace, name, snapshot_id, repo).await?
    {
        return Ok(action);
    }

    // §1 placement: the repository's home namespace (or the operator namespace for
    // a ClusterRepository); orphan when nothing survivable can host the Job.
    let job_ns = match batch_job_placement(
        repo.repo_namespace.as_deref(),
        ctx.operator_namespace.as_deref(),
        ns_terminating.then_some(namespace),
    ) {
        DeleteJobPlacement::RunIn(ns) => ns,
        DeleteJobPlacement::OrphanFallback { reason } => {
            return orphan_snapshot(backup, ctx, api, namespace, name, &reason).await;
        }
    };

    // Fail-safe store gate: never fire (or reap against) an unsynced SNAPSHOT
    // store — a cold cache must never read as "nothing pending". The breaker
    // already defers external-destructive CRs, but PRUNES reach here un-gated, so
    // the dispatcher repeats the check (same requeue as `StoresNotSynced`).
    //
    // NOTE (IMPORTANT-3): this gate INTENTIONALLY covers only the snapshot store,
    // not the SCHEDULE store. The plan originally gated both, but a hard
    // schedule-store gate here would defer EVERY deletion (prunes included) on a
    // cluster that legitimately has zero SnapshotSchedules until the empty-cluster
    // startup sync flips the flag. The refined design instead lets the FIRE set's
    // `fire_eligible` narrow just the schedule-OWNED members while the schedule
    // store is unsynced (`schedule_synced` → IMPORTANT-3a), so an unowned
    // deletion is never blocked on the schedule store's readiness.
    use std::sync::atomic::Ordering;
    let present = ctx.snapshot_store.get().is_some();
    let synced = ctx.snapshot_synced.load(Ordering::Acquire);
    let Some(store) = ctx
        .snapshot_store
        .get()
        .filter(|_| breaker_stores_ready(present, synced))
    else {
        tracing::info!(backup = %name, "snapshot store not synced yet; deferring batch deletion (no destructive work)");
        return Ok(Action::requeue(Duration::from_secs(15)));
    };

    dispatch_batch_for_member(
        backup,
        ctx,
        api,
        namespace,
        name,
        snapshot_id,
        &job_ns,
        repo_ref,
        repo,
        store,
    )
    .await
}

/// §2/§3 of [`drive_batch_deletion`], split out to keep that entry point under the
/// complexity ratchet: LIST this repository's batch Jobs ONCE, reap the drained/
/// aged terminal ones, classify THIS CR against that SAME list, and act — release
/// on a covered SUCCEEDED Job, back off on a FAILED one, poll a LIVE one, else
/// decide whether to fire a fresh wave ([`fire_batch`]).
#[allow(clippy::too_many_arguments)]
async fn dispatch_batch_for_member(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
    snapshot_id: &str,
    job_ns: &str,
    repo_ref: &RepositoryRef,
    repo: &ResolvedRepository,
    store: &Store<Snapshot>,
) -> Result<Action> {
    let state = store.state();
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), job_ns);
    let views = list_repo_batch_jobs(&job_api, repo_ref).await?;
    // Reap the drained/aged terminal Jobs (the SINGLE whole-Job metric point),
    // then classify THIS CR against the SAME list.
    reap_batch_jobs(&job_api, ctx, &views, &finalizer_holding_uids(&state)).await;
    let uid = backup.uid().unwrap_or_default();
    match member_disposition(&uid, &views) {
        MemberDisposition::LiveMember => {
            Ok(Action::requeue(deletion_requeue(DeletionRequeue::LiveJob)))
        }
        MemberDisposition::SucceededMember => {
            io::remove_finalizer(api, backup, SNAPSHOT_CLEANUP_FINALIZER).await?;
            ctx.metrics
                .inc_snapshot_deletion(namespace, SnapshotDeletionOutcome::Deleted);
            ctx.metrics
                .inc_snapshot_delete_batch_members(BatchMemberOutcome::Deleted, 1);
            tracing::info!(backup = %name, %snapshot_id, "snapshot deleted by batch Job; finalizer removed");
            Ok(Action::await_change())
        }
        MemberDisposition::FailedMember => {
            ctx.metrics.inc_snapshot_deletion_failure(namespace);
            io::patch_status(api, name, serde_json::json!({ "phase": "Deleting" })).await?;
            tracing::warn!(backup = %name, "batch delete Job failed for this member; backing off");
            Ok(Action::requeue(deletion_requeue(
                DeletionRequeue::JobFailed,
            )))
        }
        // Not (yet) enrolled: decide whether to fire a fresh wave for this repo.
        MemberDisposition::NotAMember { .. } => {
            fire_batch(
                backup, ctx, api, namespace, name, job_ns, repo_ref, repo, &state, &views,
            )
            .await
        }
    }
}

/// §0 upgrade shim: honor a legacy per-CR delete Job from a pre-batch operator.
/// Checks BOTH placements the old [`delete_job_placement`] used — `{name}-delete`
/// in the CR's own namespace (the common non-terminating case), then the capped
/// cross-namespace `{ns}-{name}-delete` in the repository's home namespace (the
/// namespace-deletion cascade). `Some(action)` means a legacy Job exists and this
/// reconcile is fully handled by the shim; `None` hands over to the batch path.
///
/// TODO(one-release): one release after batching ships, remove this adoption
/// shim AND the now-unused per-CR [`super::plan::delete_job_placement`] it stands
/// in for — no operator that has run the batch dispatcher will ever create a
/// `{name}-delete` Job again, so the shim only has to bridge a single in-place
/// upgrade. (Tracked in the mass-deletion release notes; a real GH issue is filed
/// at PR time.)
async fn adopt_legacy_delete_job(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
    snapshot_id: &str,
    repo: &ResolvedRepository,
) -> Result<Option<Action>> {
    // Location 1: same-namespace `{name}-delete` (the common non-terminating placement).
    if let Some(action) = try_legacy_delete_job(
        backup,
        ctx,
        api,
        namespace,
        name,
        snapshot_id,
        namespace,
        &format!("{name}-delete"),
        false,
    )
    .await?
    {
        return Ok(Some(action));
    }
    // Location 2: cross-namespace cascade `{ns}-{name}-delete` (capped) in the
    // repository's home namespace (namespaced Repository → its ns; ClusterRepository
    // → the operator ns), only when that differs from the CR's own namespace.
    let repo_home = repo
        .repo_namespace
        .as_deref()
        .or(ctx.operator_namespace.as_deref());
    if let Some(home) = repo_home.filter(|h| *h != namespace) {
        let capped = capped_name(&format!("{namespace}-{name}-delete"));
        if let Some(action) = try_legacy_delete_job(
            backup,
            ctx,
            api,
            namespace,
            name,
            snapshot_id,
            home,
            &capped,
            true,
        )
        .await?
        {
            return Ok(Some(action));
        }
    }
    Ok(None)
}

/// GET one legacy delete Job by name in `job_ns` and honor its terminal state with
/// the EXACT pre-batch semantics: succeeded → release the finalizer (+ reap a
/// cross-namespace Job and its work-spec ConfigMap); failed → failure metric,
/// phase `Deleting`, back off 60s; running → poll 15s. `None` when no such Job.
#[allow(clippy::too_many_arguments)]
async fn try_legacy_delete_job(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
    snapshot_id: &str,
    job_ns: &str,
    job_name: &str,
    cross_namespace: bool,
) -> Result<Option<Action>> {
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), job_ns);
    let Some(job) = job_api.get_opt(job_name).await? else {
        return Ok(None);
    };
    let action = match job_terminal_state(&job) {
        Some(true) => {
            io::remove_finalizer(api, backup, SNAPSHOT_CLEANUP_FINALIZER).await?;
            // A cross-namespace Job is not GC'd with the Snapshot (its owner is the
            // longer-lived repository CR) — reap it and its work-spec ConfigMap now.
            if cross_namespace {
                let _ = job_api.delete(job_name, &DeleteParams::background()).await;
                let cm_api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), job_ns);
                let _ = cm_api.delete(job_name, &DeleteParams::default()).await;
            }
            ctx.metrics
                .inc_snapshot_deletion(namespace, SnapshotDeletionOutcome::Deleted);
            tracing::info!(backup = %name, %snapshot_id, "snapshot deleted by legacy delete Job; finalizer removed");
            Action::await_change()
        }
        Some(false) => {
            ctx.metrics.inc_snapshot_deletion_failure(namespace);
            io::patch_status(api, name, serde_json::json!({ "phase": "Deleting" })).await?;
            tracing::warn!(backup = %name, "legacy snapshot delete Job failed; backing off");
            Action::requeue(Duration::from_secs(60))
        }
        None => Action::requeue(Duration::from_secs(15)),
    };
    Ok(Some(action))
}

/// LIST this repository's batch delete Jobs in the placement namespace (label
/// selector: managed-by + the batch op + the repo hash) and reduce each to a pure
/// [`BatchJobView`] the classifiers operate on.
async fn list_repo_batch_jobs(
    job_api: &Api<Job>,
    repo_ref: &RepositoryRef,
) -> Result<Vec<BatchJobView>> {
    let selector = format!(
        "{MANAGED_BY_LABEL}={MANAGED_BY_VALUE},{OP_LABEL}={OP_SNAPSHOT_DELETE_BATCH},{DELETE_REPO_LABEL}={}",
        repo_label(repo_ref)
    );
    let jobs = job_api
        .list(&ListParams::default().labels(&selector))
        .await?
        .items;
    Ok(jobs.iter().filter_map(batch_job_view).collect())
}

/// The member `Snapshot` UIDs a batch Job covers, parsed from its
/// [`DELETE_MEMBERS_ANNOTATION`] (comma-joined). Empty ⇒ not a batch Job / no
/// members. Shared with [`crate::sweep`]'s leak backstop.
pub(crate) fn batch_job_members(job: &Job) -> Vec<String> {
    job.annotations()
        .get(DELETE_MEMBERS_ANNOTATION)
        .map(|v| {
            v.split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Reduce a batch delete `Job` to the pure [`BatchJobView`] the classifiers use.
/// `None` when the Job carries no member annotation (not one of ours / malformed).
fn batch_job_view(job: &Job) -> Option<BatchJobView> {
    let members = batch_job_members(job);
    if members.is_empty() {
        return None;
    }
    let state = match job_terminal_state(job) {
        None => BatchJobState::Live,
        Some(true) => BatchJobState::Succeeded,
        Some(false) => BatchJobState::Failed,
    };
    Some(BatchJobView {
        name: job.name_any(),
        members,
        state,
        terminal_at: job_terminal_at(job),
    })
}

/// When a Job went terminal: its `completionTime` (success) else the `Failed`
/// condition's transition time. `None` while running (or a status missing both) —
/// [`reap_targets`] treats that as not-yet-old-enough.
fn job_terminal_at(job: &Job) -> Option<chrono::DateTime<chrono::Utc>> {
    let status = job.status.as_ref()?;
    if let Some(ct) = status.completion_time.as_ref() {
        return chrono::DateTime::from_timestamp(ct.0.as_second(), 0);
    }
    status
        .conditions
        .as_ref()?
        .iter()
        .find(|c| c.type_ == "Failed" && c.status == "True")
        .and_then(|c| c.last_transition_time.as_ref())
        .and_then(|t| chrono::DateTime::from_timestamp(t.0.as_second(), 0))
}

/// The UIDs of `Snapshot`s in the store still holding the cleanup finalizer — the
/// "not yet drained" set gating a SUCCEEDED batch Job's reap.
fn finalizer_holding_uids(snapshots: &[Arc<Snapshot>]) -> HashSet<String> {
    snapshots
        .iter()
        .filter(|s| {
            s.finalizers()
                .iter()
                .any(|f| f == SNAPSHOT_CLEANUP_FINALIZER)
        })
        .filter_map(|s| s.uid())
        .collect()
}

/// Delete the terminal batch Jobs eligible for reaping (pure [`reap_targets`]),
/// bumping the whole-Job outcome metric at this single point. Best-effort per Job:
/// a 404 (already reaped by a sibling reconcile) is silent, any other error logs
/// and is retried next pass. Batch Jobs carry NO `ttlSecondsAfterFinished`, so a
/// member reconcile can always observe the terminal Job before it is reaped here.
async fn reap_batch_jobs(
    job_api: &Api<Job>,
    ctx: &Context,
    views: &[BatchJobView],
    holding: &HashSet<String>,
) {
    for target in reap_targets(views, holding, chrono::Utc::now(), FAILED_BATCH_REAP_AGE) {
        match job_api
            .delete(&target.name, &DeleteParams::background())
            .await
        {
            Ok(_) => {
                ctx.metrics.inc_snapshot_delete_batch_job(target.outcome);
                // The FAILED per-member metric is emitted ONCE here (its members
                // never drain their own finalizers, so they have no per-member
                // site). A SUCCEEDED Job's members each emit `deleted` as they
                // drain (`dispatch_batch_for_member`), so nothing is counted here
                // for it. Exhaustive over the outcome so a new one must decide.
                match target.outcome {
                    BatchJobOutcome::Failed => ctx.metrics.inc_snapshot_delete_batch_members(
                        BatchMemberOutcome::Failed,
                        target.members as u64,
                    ),
                    BatchJobOutcome::Succeeded => {}
                }
            }
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => {
                tracing::warn!(job = %target.name, error = %e, "batch delete Job reap failed (skipped)")
            }
        }
    }
}

/// Resolving pending-delete candidate namespaces' terminating state, split by
/// READ CONFIDENCE so the plan/count path and the FIRE path treat an UNREADABLE
/// namespace differently — they must (see the field docs).
struct TerminatingNamespaces {
    /// Namespaces a successful GET proved TERMINATING (`Ok(true)`). ONLY these
    /// feed plan evaluation ([`batch::pending_members`]) and breaker
    /// counting-as-terminating: an unreadable namespace must NOT flip a
    /// retain-destined draining child into a destructive `DeleteSnapshot` member
    /// in a namespace that is NOT being deleted. This keeps the peer-driven batch
    /// path symmetric with the finalizer's self-CR path, which resolves the same
    /// read to NOT-terminating (`unwrap_or(false)`).
    confirmed: HashSet<String>,
    /// Namespaces whose terminating-ness could NOT be read (`Err`). EXCLUDED from
    /// plan evaluation / counting-as-terminating (a retain-cascade child keeps
    /// retain semantics), but folded into the FIRE path's ns-Orphan exclusion (via
    /// [`Self::orphan_exclusion`]) AND excluded from the FIRE set OUTRIGHT: a peer
    /// whose namespace we cannot confirm terminating is never deleted on the
    /// assumption that it is — its own self-fire (authoritative LIVE resolution)
    /// drains it. Counting such a member toward the breaker is still fine
    /// (over-count is fail-safe).
    unreadable: HashSet<String>,
}

impl TerminatingNamespaces {
    /// `confirmed ∪ unreadable` — the set fed to the FIRE path's ns-Orphan
    /// exclusion ([`batch::FireEligibility::terminating_namespaces`]): an
    /// `onNamespaceDelete: Orphan` peer whose namespace is terminating OR unreadable
    /// plans `OrphanSnapshot` on its OWN reconcile and must never be deleted by a
    /// peer's fire.
    fn orphan_exclusion(&self) -> HashSet<String> {
        self.confirmed.union(&self.unreadable).cloned().collect()
    }
}

/// Resolve, once per DISTINCT namespace, whether it is TERMINATING (the same
/// `namespace_is_terminating` check the finalizer path uses for the self CR),
/// splitting the result into CONFIRMED-terminating (`Ok(true)`) and UNREADABLE
/// (`Err`) sets. `namespaces` are the pending-delete CANDIDATE namespaces
/// ([`batch::pending_candidate_namespaces`]), resolved BEFORE the counting set is
/// built so each member's plan is evaluated in its real `(ns_terminating,
/// ns_policy)` form. Bounded — a candidate must carry a `deletionTimestamp`, so
/// this is the set of CRs actually being deleted for the repo, not the whole store.
///
/// The Err direction is NOT treated as terminating for plan/count (that would let
/// a transient read error flip a retain-destined child into a destructive external
/// delete under `onNamespaceDelete: Delete` — deleting data Retain promised to keep
/// in a namespace that is NOT being deleted). Instead, an unreadable namespace is
/// (a) excluded from plan/count-as-terminating — matching the self-CR path's
/// `unwrap_or(false)` — while still (b) protecting its `Orphan` peers on the fire
/// path (via [`TerminatingNamespaces::orphan_exclusion`]) and (c) withheld from the
/// FIRE set outright. (A 403 in a namespaced-scope install resolves to `Ok(false)`
/// — "not terminating" — inside `namespace_is_terminating`, so it lands in NEITHER
/// set and never wedges an install that legitimately cannot read Namespaces.)
async fn resolve_terminating_namespaces(
    ctx: &Context,
    namespaces: &std::collections::BTreeSet<String>,
) -> TerminatingNamespaces {
    let mut confirmed = HashSet::new();
    let mut unreadable = HashSet::new();
    for ns in namespaces {
        match io::namespace_is_terminating(&ctx.client, ns).await {
            Ok(true) => {
                confirmed.insert(ns.clone());
            }
            Ok(false) => {}
            Err(_) => {
                unreadable.insert(ns.clone());
            }
        }
    }
    TerminatingNamespaces {
        confirmed,
        unreadable,
    }
}

/// §3: this CR is not enrolled — pick the fireable set for its repository
/// (`pending` minus the covered UIDs: members of LIVE jobs — the no-overlap
/// invariant — plus members of SUCCEEDED jobs awaiting finalizer release, so a
/// second wave never re-enrolls an already-deleted member; FAILED members stay
/// fireable for retry), and either fire a wave (throttle permitting) or requeue.
/// `state` is the store snapshot the classifier used, so the covered set
/// matches the LIST exactly.
#[allow(clippy::too_many_arguments)]
async fn fire_batch(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
    job_ns: &str,
    repo_ref: &RepositoryRef,
    repo: &ResolvedRepository,
    state: &[Arc<Snapshot>],
    views: &[BatchJobView],
) -> Result<Action> {
    use std::sync::atomic::Ordering;
    let key = repo_key(repo_ref);
    // Resolve which candidate namespaces are terminating BEFORE building the
    // counting set, so each member's plan is evaluated in its real
    // `(ns_terminating, ns_policy)` form. The resolution splits into CONFIRMED
    // (`Ok(true)`) and UNREADABLE (`Err`): ONLY confirmed feeds the plan/count (a
    // retain-cascade member — a `policy-cascade` stamp, or a schedule-cascade
    // Retain — flips to a destructive external delete under `onNamespaceDelete:
    // Delete` only when its namespace is CONFIRMED terminating, so it must then be
    // counted AND become fireable, exactly like an unstamped external child). An
    // unreadable namespace stays retain-form here (C1) — it must not flip a retain
    // child into a destructive delete in a namespace that is NOT being deleted.
    let terminating =
        resolve_terminating_namespaces(ctx, &pending_candidate_namespaces(state, &key)).await;
    // COUNTING set: maximally inclusive (over-count is fail-safe). Confirmed-only
    // for the plan; an unstamped external member in an UNREADABLE namespace still
    // lands here (its plan is `DeleteSnapshot` regardless of ns state) and counts
    // toward the breaker — over-count stays safe.
    let pending = pending_members(
        state,
        &key,
        schedule_owner_lookup(ctx),
        &terminating.confirmed,
        repo.on_namespace_delete,
    );
    // FIRE set: maximally EXCLUSIVE (under-fire is fail-safe). Drop members the
    // count deliberately over-includes — breaker-HELD externals (CRITICAL-1),
    // ns-Orphan-destined peers (IMPORTANT-2) plus any UNREADABLE-namespace peer
    // outright, schedule-owned peers while the schedule store is unsynced
    // (IMPORTANT-3a), and unpinned PEERS (IMPORTANT-4) — BEFORE the no-overlap
    // exclusion. The ns-Orphan exclusion sees `confirmed ∪ unreadable` (an Orphan
    // peer in a terminating-or-unreadable namespace is withheld); the unreadable
    // set additionally withholds ANY peer there, whatever the policy. The ack is
    // parsed exactly as the breaker does; its invalid-value warning is already
    // published by `resolve_breaker` on the external-CR path, so it is not
    // re-emitted here.
    let now = chrono::Utc::now();
    let (ack, _invalid) = parse_mass_deletion_ack(repo.mass_deletion_ack.as_deref(), now);
    let threshold =
        kopiur_api::consts::effective_mass_deletion_threshold(repo.deletion_protection.as_ref());
    let self_uid = backup.uid().unwrap_or_default();
    let orphan_exclusion = terminating.orphan_exclusion();
    let eligible = fire_eligible(
        pending,
        &FireEligibility {
            self_uid: &self_uid,
            threshold,
            ack,
            on_namespace_delete: repo.on_namespace_delete,
            terminating_namespaces: &orphan_exclusion,
            unreadable_namespaces: &terminating.unreadable,
            schedule_synced: ctx.schedule_synced.load(Ordering::Acquire),
        },
    );
    let fireable = fireable_members(eligible, &covered_uids(views));
    if fireable.is_empty() {
        // This CR is not (yet) an eligible member — e.g. its own pending state has
        // not propagated to the store, or every pending member is already in flight.
        // Poll on the live-batch cadence; NEVER release the finalizer here (this
        // CR's kopia delete has not happened).
        return Ok(Action::requeue(deletion_requeue(DeletionRequeue::LiveJob)));
    }
    match batch_fire_decision(
        &fireable,
        chrono::Utc::now(),
        BATCH_QUIET_WINDOW,
        MAX_BATCH_MEMBERS,
    ) {
        BatchFire::Accumulate { retry_in } => Ok(Action::requeue(deletion_requeue(
            DeletionRequeue::Accumulating(retry_in),
        ))),
        BatchFire::Fire(members) => match throttle_verdict(
            throttle_live_count(ctx).await?,
            ctx.max_concurrent_delete_jobs,
        ) {
            ThrottleVerdict::Wait => Ok(Action::requeue(deletion_requeue(
                DeletionRequeue::Throttled,
            ))),
            ThrottleVerdict::Proceed => {
                launch_batch_job(
                    backup, ctx, api, namespace, name, job_ns, repo_ref, repo, &members,
                )
                .await
            }
        },
    }
}

/// Count the LIVE batch delete Jobs across the whole watch scope — the throttle's
/// cluster-wide concurrency input. Skipped entirely (returns 0) when UNCAPPED (the
/// default), so a normal install never pays for the extra LIST.
async fn throttle_live_count(ctx: &Context) -> Result<usize> {
    if ctx.max_concurrent_delete_jobs.is_none() {
        return Ok(0);
    }
    let selector =
        format!("{MANAGED_BY_LABEL}={MANAGED_BY_VALUE},{OP_LABEL}={OP_SNAPSHOT_DELETE_BATCH}");
    let job_api: Api<Job> = crate::controllers::scoped_api(&ctx.client, &ctx.watch_scope);
    let jobs = job_api
        .list(&ListParams::default().labels(&selector))
        .await?
        .items;
    Ok(jobs
        .iter()
        .filter(|j| job_terminal_state(j).is_none())
        .count())
}

/// Build (§2) and CREATE the batch delete Job for `members`, then move THIS CR to
/// `Deleting` (clearing a prior `DeletionHeld` in the same patch) and poll. A 409
/// `AlreadyExists` means a sibling reconcile already CREATEd the same-named Job
/// (deterministic member-set name) — NEVER SSA-force over it (that could rewrite a
/// live Job's delete-members annotation); poll instead.
#[allow(clippy::too_many_arguments)]
async fn launch_batch_job(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
    job_ns: &str,
    repo_ref: &RepositoryRef,
    repo: &ResolvedRepository,
    members: &[PendingMember],
) -> Result<Action> {
    let job_name = batch_job_name(repo_ref, members);
    let job = build_batch_job(
        ctx, job_ns, &job_name, repo_ref, repo, members, namespace, name,
    )
    .await?;
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), job_ns);
    match job_api.create(&PostParams::default(), &job).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 409 => {
            return Ok(Action::requeue(deletion_requeue(DeletionRequeue::LiveJob)));
        }
        Err(e) => return Err(Error::Kube(e)),
    }
    // Move THIS CR to Deleting; if it was HELD by the breaker (now proceeding on an
    // acknowledged wave), clear `DeletionHeld=False` in the SAME patch.
    let mut deleting = serde_json::json!({ "phase": "Deleting" });
    if let Some(conds) = cleared_held_conditions(backup) {
        deleting["conditions"] = serde_json::to_value(&conds).unwrap_or_default();
    }
    io::patch_status(api, name, deleting).await?;
    tracing::info!(
        backup = %name,
        job = %job_name,
        members = members.len(),
        job_namespace = %job_ns,
        "created SnapshotDeleteBatch Job"
    );
    Ok(Action::requeue(deletion_requeue(DeletionRequeue::LiveJob)))
}

/// §2 batch Job construction. Mirrors the retired per-CR delete Job's semantics
/// N-at-a-time: pin-first repository resolution (done by the caller), projection
/// HARDCODED OFF (the batch runs where the repository's canonical Secret already
/// lives), the repository's `moverDefaults` inheritance, and RBAC minting — but
/// with per-member items, NO `ttlSecondsAfterFinished` (explicit reaping), the
/// sorted-UID members annotation, and an ownerRef ONLY for a namespaced
/// `Repository` (a `ClusterRepository`'s cluster-scoped owner would be an
/// un-GC'able ref on a namespaced Job — labels + the reaper stand in instead).
///
/// `src_namespace`/`src_name` are the TRIGGERING CR's coordinates, used only to
/// enrich a credentials-resolution error with the stuck-finalizer escape hatch.
#[allow(clippy::too_many_arguments)]
async fn build_batch_job(
    ctx: &Context,
    job_ns: &str,
    job_name: &str,
    repo_ref: &RepositoryRef,
    repo: &ResolvedRepository,
    members: &[PendingMember],
    src_namespace: &str,
    src_name: &str,
) -> Result<Job> {
    let items: Vec<SnapshotDeleteItem> = members
        .iter()
        .map(|m| SnapshotDeleteItem {
            snapshot_id: m.snapshot_id.clone(),
            anchor: m.anchor.clone(),
        })
        .collect();
    // A ClusterRepository owner is cluster-scoped — the resulting ownerRef on a
    // namespaced Job is invalid and never GC'd. Only a namespaced Repository owns
    // its batch Job; the ownerRef is stripped below for a ClusterRepository.
    let owner = repo.owner_ref.clone();
    // Creds in the placement namespace with projection HARDCODED OFF (same
    // rationale as the retired cross-namespace path): the batch runs at the
    // repository's home, where its canonical Secret already lives, so no copy is
    // needed — and projecting would mint one owned by a possibly cluster-scoped
    // repository CR (an un-GC'able leak). Enrich a credentials error with the
    // stuck-finalizer escape hatch, exactly as the retired path did.
    let creds = io::resolve_mover_creds_for(
        &ctx.client,
        job_ns,
        &io::CredsPrefix::snapshot_delete_batch(job_name),
        &owner,
        repo,
        false,
        io::repo_kind_str(repo_ref.kind),
        &repo_ref.name,
    )
    .await
    .map_err(|e| hint_deletion_blocker(e, src_namespace, src_name))?;
    if creds.projected > 0 {
        ctx.metrics.inc_secrets_projected(job_ns, creds.projected);
    }
    let creds_secrets = io::plain_creds(creds.names);
    let work_spec = MoverWorkSpec {
        version: 1,
        operation: Operation::SnapshotDeleteBatch(SnapshotDeleteBatchOp { items }),
        // Identity only satisfies the work-spec SHAPE — a batch deletes by manifest
        // id (+ per-item anchor), never by identity — so the FIRST member's pinned
        // identity (its anchor) is a correct, representative value.
        identity: batch_identity(members),
        repository: repository_connect(repo)?,
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            // The reporter is selected mover-side (log-only); no /status RBAC.
            kind: "SnapshotDeleteBatch".to_string(),
            name: job_name.to_string(),
            namespace: job_ns.to_string(),
        },
        hook_plan: Default::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    // Labels: managed-by (added by `build_job`) + the batch op + the repo hash (so
    // the dispatcher can LIST one repository's batch Jobs). Annotation: the SORTED
    // member UID set — the single source of truth for "which Snapshots".
    let mut labels = BTreeMap::from([
        (OP_LABEL.to_string(), OP_SNAPSHOT_DELETE_BATCH.to_string()),
        (DELETE_REPO_LABEL.to_string(), repo_label(repo_ref)),
    ]);
    let mut uids: Vec<&str> = members.iter().map(|m| m.uid.as_str()).collect();
    uids.sort_unstable();
    let annotations = BTreeMap::from([(DELETE_MEMBERS_ANNOTATION.to_string(), uids.join(","))]);
    let repo_volume =
        io::filesystem_repo_mount_source(&repo.backend).map(|source| VolumeMountSpec {
            source,
            mount_path: io::filesystem_repo_path(&repo.backend).unwrap_or_default(),
            read_only: false,
        });
    // Inherit the repository's moverDefaults (security context, placement) so the
    // batch can reach a filesystem/NFS repo on a non-65532-owned directory.
    let resolved_mover = kopiur_api::common::resolve_mover(
        repo.mover_defaults.as_ref(),
        None,
        None,
        None,
        None,
        None,
    );
    let limits = JobLimits {
        // NO TTL — reaping is EXPLICIT (the dispatcher + sweep own it), so a member
        // reconcile can always observe the terminal Job.
        ttl_seconds_after_finished: None,
        ..JobLimits::default()
    };
    let mover_identity = io::ensure_mover_identity(
        &ctx.client,
        job_ns,
        &[&repo.backend],
        ctx.mover_service_account.as_deref(),
        ctx.mover_role_kind.as_str(),
        &ctx.mover_clusterrole,
    )
    .await?;
    mover_identity.decorate_labels(&mut labels);
    let inputs = MoverJobInputs {
        name: job_name,
        namespace: job_ns,
        owner,
        work_spec: &work_spec,
        image: &ctx.mover_image,
        image_pull_policy: ctx.mover_pull_policy(),
        limits,
        resources: resolved_mover.resources.clone(),
        security_context: resolved_mover.security_context.clone(),
        pod_security_context: resolved_mover.pod_security_context.clone(),
        node_selector: resolved_mover.node_selector.clone(),
        tolerations: resolved_mover.tolerations.clone(),
        affinity: resolved_mover.affinity.clone(),
        labels,
        source_volume: None,
        repo_volume,
        creds_secrets,
        result_configmap: None,
        service_account: mover_identity.service_account.as_deref(),
        passthrough_env: ctx.mover_env_passthrough.clone(),
        extra_env: Vec::new(),
        annotations,
        cache_volume: Default::default(),
        scratch_volume: None,
        readiness_exec: None,
    };
    let mut job = jobs::build_job(&inputs)?;
    // ClusterRepository: drop the cluster-scoped (un-GC'able) ownerRef; the Job is
    // reaped by label + the explicit reaper instead.
    if repo_ref.kind == RepositoryKind::ClusterRepository {
        job.metadata.owner_references = None;
    }
    Ok(job)
}

/// The work-spec identity for a batch Job, taken from the FIRST member's pinned
/// anchor (source path + recorded username/hostname). Shape-only: a batch deletes
/// by manifest id, so this never drives a kopia connect — it satisfies the
/// `MoverWorkSpec` shape with a real, representative identity.
fn batch_identity(members: &[PendingMember]) -> kopiur_mover::workspec::ResolvedIdentity {
    let anchor = members.first().map(|m| &m.anchor);
    kopiur_mover::workspec::ResolvedIdentity {
        username: anchor.and_then(|a| a.username.clone()).unwrap_or_default(),
        hostname: anchor.and_then(|a| a.hostname.clone()).unwrap_or_default(),
        source_path: anchor.map(|a| a.source_path.clone()).unwrap_or_default(),
    }
}

/// Reconcile kopia's snapshot-pin state with `Snapshot.spec.pin` (ADR-0005 §13(c)),
/// after the snapshot exists. Issues a `SnapshotPin` mover Job only when the desired
/// (`spec.pin`) and observed (`status.pinned`) state differ (the tested
/// [`pin_decision`]); on the Job's terminal success it records the new observed pin
/// state in `status.pinned`. A `NoOp` (or a not-yet-recorded snapshot id) just
/// returns the standard succeeded-snapshot requeue.
async fn reconcile_pin(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
    // The origin `reconcile_inner` already resolved (always `Scheduled` or
    // `Manual` — the catalog origins return from their pin arms long before
    // this). Threaded through rather than re-resolved so this Job-label stamp
    // can never disagree with the dispatch decision (and never needs a default
    // for an unparseable label, which cannot reach here).
    origin: Origin,
) -> Result<Action> {
    let desired = backup.spec.pin;
    let observed = backup.status.as_ref().and_then(|s| s.pinned);
    let steady = Action::requeue(TERMINAL_SNAPSHOT_STEADY_REQUEUE);
    let action = pin_decision(desired, observed);
    let job_name = format!("{name}-pin");

    // Cost gate: the common never-pinned Snapshot (spec.pin unset, no pin ever
    // ran) skips the per-pass Job GET entirely. A pin mover may only exist once
    // one was spawned, and spawning durably marks the CR first (the `Pinned`
    // condition, upserted BEFORE the Job is applied) — so this gate can never
    // hide a leftover pin Job, including one from a mid-flight spec toggle
    // back to `pin: false` before the mover finished.
    if action == PinAction::NoOp && !pin_job_may_exist(backup) {
        return Ok(steady);
    }

    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    if let Some(job) = job_api.get_opt(&job_name).await? {
        return handle_pin_job(backup, ctx, api, namespace, name, &job_name, &job, action).await;
    }
    if action == PinAction::NoOp {
        return Ok(steady);
    }
    // Need the kopia snapshot id to pin/unpin; it's recorded once Succeeded.
    let Some(snapshot_id) = backup
        .status
        .as_ref()
        .and_then(|s| s.snapshot.as_ref())
        .map(|s| s.kopia_snapshot_id.clone())
    else {
        // Not resolved yet (e.g. object-store mover hasn't PATCHed the id) — retry.
        return Ok(Action::requeue(Duration::from_secs(30)));
    };

    // Create the SnapshotPin Job (mirrors the SnapshotDelete one-shot path).
    let (config, effective_repo, repo) = resolve_recipe(ctx, backup, namespace).await?;
    // The child's effective repository (pin-aware — a pinned child of a
    // multi-repo policy pins/unpins in ITS repository).
    let repo_ref = &effective_repo;
    let identity = resolve_identity_for(
        &config,
        namespace,
        repo.identity_defaults.as_ref(),
        backup.spec.source.as_ref(),
    )?;
    let owner = io::owner_ref_for(backup, "Snapshot")?;
    let creds = io::resolve_mover_creds_for(
        &ctx.client,
        namespace,
        &io::CredsPrefix::snapshot_pin(name),
        &owner,
        &repo,
        config
            .spec
            .credential_projection
            .as_ref()
            .is_some_and(|p| p.enabled),
        io::repo_kind_str(repo_ref.kind),
        &repo_ref.name,
    )
    .await?;
    if creds.projected > 0 {
        ctx.metrics
            .inc_secrets_projected(namespace, creds.projected);
    }
    let creds_secrets = io::plain_creds(creds.names);
    let work_spec = MoverWorkSpec {
        version: 1,
        operation: Operation::SnapshotPin(SnapshotPinOp {
            snapshot_id: snapshot_id.clone(),
            pin: matches!(action, PinAction::Pin),
            // kopia rewrites the manifest id on (un)pin; the mover re-resolves
            // the live id by these anchors and re-stamps status.snapshot.
            anchor: snapshot_anchor(backup),
        }),
        identity,
        repository: repository_connect(&repo)?,
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            kind: "Snapshot".to_string(),
            name: name.to_string(),
            namespace: namespace.to_string(),
        },
        hook_plan: Default::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: Default::default(),
    };
    let mut labels = run_labels(&config, origin);
    labels.insert(
        "kopiur.home-operations.com/op".to_string(),
        "snapshot-pin".to_string(),
    );
    // Stamp the direction the Job APPLIES, so its terminal state is later
    // reconciled by what it did — not by whatever spec.pin says at that moment.
    let pin_target = matches!(action, PinAction::Pin);
    let annotations = BTreeMap::from([(
        crate::consts::PIN_TARGET_ANNOTATION.to_string(),
        pin_target.to_string(),
    )]);
    let repo_volume =
        io::filesystem_repo_mount_source(&repo.backend).map(|source| VolumeMountSpec {
            source,
            mount_path: io::filesystem_repo_path(&repo.backend).unwrap_or_default(),
            read_only: false,
        });
    let resolved_mover = kopiur_api::common::resolve_mover(
        repo.mover_defaults.as_ref(),
        None,
        None,
        None,
        None,
        None,
    );
    let limits = JobLimits {
        ttl_seconds_after_finished: resolved_mover.ttl_seconds_after_finished,
        ..JobLimits::default()
    };
    let mover_identity = io::ensure_mover_identity(
        &ctx.client,
        namespace,
        &[&repo.backend],
        ctx.mover_service_account.as_deref(),
        ctx.mover_role_kind.as_str(),
        &ctx.mover_clusterrole,
    )
    .await?;
    mover_identity.decorate_labels(&mut labels);
    let inputs = MoverJobInputs {
        name: &job_name,
        namespace,
        owner,
        work_spec: &work_spec,
        image: &ctx.mover_image,
        image_pull_policy: ctx.mover_pull_policy(),
        limits,
        resources: resolved_mover.resources.clone(),
        security_context: resolved_mover.security_context.clone(),
        pod_security_context: resolved_mover.pod_security_context.clone(),
        node_selector: resolved_mover.node_selector.clone(),
        tolerations: resolved_mover.tolerations.clone(),
        affinity: resolved_mover.affinity.clone(),
        labels,
        source_volume: None,
        repo_volume,
        creds_secrets,
        result_configmap: None,
        service_account: mover_identity.service_account.as_deref(),
        passthrough_env: ctx.mover_env_passthrough.clone(),
        extra_env: Vec::new(),
        annotations,
        cache_volume: Default::default(),
        scratch_volume: None,
        readiness_exec: None,
    };
    let job = jobs::build_job(&inputs)?;
    // Durably mark "a pin mover exists" BEFORE the Job lands (the `Pinned`
    // condition is the gate that decides whether steady passes look for one) —
    // a crash between the two leaves only a spurious per-pass GET, never an
    // unobserved Job.
    let conditions = io::upsert_condition_status(
        &fresh_conditions(api, name, backup).await,
        crate::consts::PINNED_CONDITION,
        "Unknown",
        "PinJobRunning",
        "a SnapshotPin mover Job is applying spec.pin",
        backup.meta().generation,
    );
    io::patch_status(api, name, serde_json::json!({ "conditions": conditions })).await?;
    io::apply_mover_objects(&ctx.client, namespace, &job_name, None, &job).await?;
    tracing::info!(backup = %name, %snapshot_id, ?action, "created SnapshotPin Job");
    Ok(Action::requeue(Duration::from_secs(15)))
}

/// Whether a `{name}-pin` mover Job may exist for this Snapshot — the cheap,
/// cache-only gate for the steady-state pin-Job lookup. True once anything pin
/// ever happened: `spec.pin` set, `status.pinned` recorded, or the `Pinned`
/// condition present (upserted BEFORE every pin Job is applied, so a mover
/// spawned for a since-reverted `spec.pin` is still findable).
fn pin_job_may_exist(backup: &Snapshot) -> bool {
    backup.spec.pin
        || backup.status.as_ref().is_some_and(|s| {
            s.pinned.is_some()
                || s.conditions
                    .iter()
                    .any(|c| c.type_ == crate::consts::PINNED_CONDITION)
        })
}

/// The pin state a `{name}-pin` Job was spawned to apply, from
/// [`crate::consts::PIN_TARGET_ANNOTATION`]. `None` for a legacy Job from an
/// operator version that didn't stamp it (its outcome can't be attributed, so
/// it is consumed without recording).
fn pin_job_target(job: &Job) -> Option<bool> {
    job.metadata
        .annotations
        .as_ref()?
        .get(crate::consts::PIN_TARGET_ANNOTATION)?
        .parse()
        .ok()
}

/// The freshest `status.conditions` for `name`, falling back to the cached
/// copy. The pin paths rewrite the whole conditions array (status writes are
/// JSON merge patches), so basing the upsert on the live object — not the
/// reflector cache — shrinks the window where a just-written condition from a
/// concurrent reconcile pass would be clobbered.
async fn fresh_conditions(
    api: &Api<Snapshot>,
    name: &str,
    backup: &Snapshot,
) -> Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    match api.get_opt(name).await {
        Ok(Some(latest)) => latest.status.map(|s| s.conditions).unwrap_or_default(),
        _ => backup
            .status
            .as_ref()
            .map(|s| s.conditions.clone())
            .unwrap_or_default(),
    }
}

/// Whether a FAILED pin Job's direction is still what the current decision
/// wants — if not (the spec toggled past it, or nothing is wanted anymore) the
/// stale Job is consumed instead of kept as a retry-backoff marker. Pure so
/// the direction-vs-decision matrix is unit-tested.
fn pin_job_still_wanted(target: Option<bool>, action: PinAction) -> bool {
    match (target, action) {
        (Some(t), PinAction::Pin) => t,
        (Some(t), PinAction::Unpin) => !t,
        // Legacy direction-less Job: assume it was this action's attempt.
        (None, PinAction::Pin | PinAction::Unpin) => true,
        (_, PinAction::NoOp) => false,
    }
}

/// The requeue after consuming a leftover pin Job: near-immediate when a pin
/// action is still pending (the spawn path runs next pass), steady otherwise.
fn post_pin_consume_action(action: PinAction) -> Action {
    match action {
        PinAction::NoOp => Action::requeue(TERMINAL_SNAPSHOT_STEADY_REQUEUE),
        PinAction::Pin | PinAction::Unpin => Action::requeue(Duration::from_secs(5)),
    }
}

/// Reconcile an EXISTING `{name}-pin` Job against the desired/observed pin
/// state. The Job carries the direction it applied ([`pin_job_target`]), so a
/// terminal Job is consumed by what it DID — never by what `spec.pin` happens
/// to say now. That closes both silent-divergence shapes: a stale succeeded
/// Job can't satisfy the opposite toggle, and a pin that completed after a
/// mid-flight spec flip is still recorded (the next pass then spawns the
/// corrective mover).
#[allow(clippy::too_many_arguments)]
async fn handle_pin_job(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
    job_name: &str,
    job: &Job,
    action: PinAction,
) -> Result<Action> {
    let observed = backup.status.as_ref().and_then(|s| s.pinned);
    let target = pin_job_target(job);
    match job_terminal_state(job) {
        // Still running — wait, whatever the current decision is (a mid-flight
        // toggle is reconciled from the terminal outcome, below).
        None => Ok(Action::requeue(Duration::from_secs(15))),
        Some(true) => match target {
            // The mover applied `t` and status doesn't say so yet: record the
            // kopia truth (even if spec.pin has since flipped — the next pass
            // recomputes the decision from the accurate observed state and
            // spawns the corrective mover). The Job is consumed on a LATER
            // pass, once this write is visible in the cache — deleting it here
            // would race the reflector (the deletion event can re-reconcile
            // against a stale `status.pinned` and respawn a spurious mover).
            Some(t) if observed != Some(t) => {
                record_pin_outcome(backup, api, name, t).await?;
                Ok(Action::requeue(Duration::from_secs(5)))
            }
            // Outcome already recorded (or a legacy direction-less Job whose
            // result can't be attributed): consume it so it can never satisfy
            // a future toggle, then act on the (now accurate) decision.
            _ => {
                io::delete_mover_run(&ctx.client, namespace, job_name).await?;
                Ok(post_pin_consume_action(action))
            }
        },
        Some(false) => {
            // The mover failed: kopia's pin state is unchanged (= `observed`).
            // A failed Job whose direction is still wanted stays until its TTL
            // — it keeps the pod logs AND is the natural retry backoff (the
            // TTL reap re-enters the spawn path; deleting it now would respawn
            // immediately on the deletion event, hot-looping a persistent
            // failure). A STALE failed Job (direction no longer wanted, or
            // nothing wanted at all) is consumed instead so it can't block —
            // or mis-satisfy — a future toggle.
            if !pin_job_still_wanted(target, action) {
                io::delete_mover_run(&ctx.client, namespace, job_name).await?;
                return Ok(post_pin_consume_action(action));
            }
            let conditions = io::upsert_condition(
                &fresh_conditions(api, name, backup).await,
                crate::consts::PINNED_CONDITION,
                observed.unwrap_or(false),
                crate::consts::PIN_JOB_FAILED_REASON,
                "the SnapshotPin mover Job failed; see the Job/pod logs",
                backup.meta().generation,
            );
            io::patch_status(api, name, serde_json::json!({ "conditions": conditions })).await?;
            tracing::warn!(backup = %name, "snapshot pin Job failed; backing off");
            Ok(Action::requeue(Duration::from_secs(120)))
        }
    }
}

/// Record a pin mover's applied state (`status.pinned` + the `Pinned`
/// condition) — the kopia truth, independent of the currently-desired spec.
async fn record_pin_outcome(
    backup: &Snapshot,
    api: &Api<Snapshot>,
    name: &str,
    pinned: bool,
) -> Result<()> {
    let conditions = io::upsert_condition(
        &fresh_conditions(api, name, backup).await,
        crate::consts::PINNED_CONDITION,
        pinned,
        "PinReconciled",
        "the SnapshotPin mover Job ran",
        backup.meta().generation,
    );
    io::patch_status(
        api,
        name,
        serde_json::json!({ "pinned": pinned, "conditions": conditions }),
    )
    .await?;
    tracing::info!(backup = %name, pin = pinned, "snapshot pin recorded");
    Ok(())
}

/// On a Job's terminal success, pin phase=Succeeded and the resulting kopia
/// snapshot id/identity into status. The controller is the authoritative source
/// of the terminal phase AND (for the filesystem backend) of the snapshot id: it
/// resolves the newest snapshot for the run's identity in-process, so status is
/// complete even when the in-cluster mover cannot PATCH back (best-effort path).
/// The mover still PATCHes stats when it can reach the API server.
async fn finalize_succeeded(
    ctx: &Context,
    backup: &Snapshot,
    api: &Api<Snapshot>,
    name: &str,
    namespace: &str,
) -> Result<()> {
    // Best-effort: re-resolve the snapshot id for the filesystem backend. This is
    // only a fallback — the authoritative `status.snapshot.kopiaSnapshotID` is
    // (1) stamped by the mover at create and (2) re-stamped by the pin mover
    // (kopia rewrites the id on pin). This in-process listing needs the repo
    // mounted into the CONTROLLER, which is usually NOT the case; on failure we
    // keep the mover-stamped create id and log the actionable hint.
    let snapshot = resolve_succeeded_snapshot(ctx, backup, namespace).await;
    // Base status carries the kstatus Ready conditions (ADR-0005 §2) so
    // `kubectl wait --for=condition=Ready` works on a Succeeded Snapshot.
    let mut status = snapshot_ready_status(
        backup,
        SnapshotPhase::Succeeded,
        "SnapshotCreated",
        "the kopia snapshot was created successfully",
    );
    match snapshot {
        Ok(Some((id, identity))) => {
            status["snapshot"] = serde_json::json!({
                "kopiaSnapshotID": id,
                "identity": identity,
            });
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                backup = %name,
                error = %e,
                "could not re-resolve the snapshot id in-process (the filesystem repo is not \
                 mounted into the controller); keeping the mover-recorded create id. Mount the \
                 repo into the controller to enable in-process resolution, or it self-corrects \
                 on the next pin reconcile",
            );
        }
    }
    io::patch_status(api, name, status).await?;
    // Terminal transition (guarded by the `phase != Succeeded` check at the call
    // site): count it exactly once. The last-success TIMESTAMP is no longer stamped
    // here — it is the mover-recorded `status.timing.endTime`, surfaced by the
    // store-backed `kopiur_snapshot_last_success_timestamp_seconds` observable gauge.
    ctx.metrics
        .inc_snapshot_completed("succeeded", namespace, backup_policy(backup));
    tracing::info!(backup = %name, "backup Job succeeded; phase=Succeeded");
    Ok(())
}

/// Resolve the newest snapshot matching this backup's identity for the
/// filesystem backend (in-process, ADR §5.4). Returns the snapshot id and a
/// status `identity` JSON body, or `None` when not resolvable in-process.
async fn resolve_succeeded_snapshot(
    ctx: &Context,
    backup: &Snapshot,
    namespace: &str,
) -> Result<Option<(String, serde_json::Value)>> {
    let (config, _effective_repo, repo) = resolve_recipe(ctx, backup, namespace).await?;
    let identity = resolve_identity_for(
        &config,
        namespace,
        repo.identity_defaults.as_ref(),
        backup.spec.source.as_ref(),
    )?;
    match &repo.backend {
        Backend::Filesystem(fs) => {
            let creds = io::repo_credentials(&repo.encryption);
            let password = io::read_repo_password(&ctx.client, namespace, &creds).await?;
            let client = ctx.kopia.build([("KOPIA_PASSWORD".to_string(), password)]);
            client
                .repository_connect(
                    &kopiur_kopia::ConnectSpec::Filesystem {
                        path: fs.path.clone().into(),
                    },
                    kopiur_kopia::CacheTuning::default(),
                )
                .await?;
            // Match the snapshot by identity, newest first: source path is
            // necessary but NOT sufficient once two sources share it (the same
            // PVC subpath repeats across namespaces, and, in a shared
            // repository, across clusters) — the recorded username/hostname
            // must match too, or this could resolve to a DIFFERENT source's
            // snapshot.
            let mut list = client.snapshot_list(None).await?;
            list.sort_by_key(|e| std::cmp::Reverse(e.end_time));
            let matched = list
                .into_iter()
                .find(|e| matches_snapshot_identity(e, &identity));
            Ok(matched.map(|e| {
                let id = e.id.clone();
                let body = serde_json::json!({
                    "username": e.source.user_name,
                    "hostname": e.source.host,
                    "sourcePath": e.source.path,
                });
                (id, body)
            }))
        }
        _ => Ok(None),
    }
}

/// Resolve a `Snapshot`'s referenced `SnapshotPolicy`, the EFFECTIVE repository
/// this child runs against, and that repository's resolved surface. Cluster
/// references and non-filesystem backends still resolve here; backend-specific
/// behavior is decided downstream.
///
/// The repository is the shared pin-aware decision
/// ([`kopiur_api::snapshot::effective_repository_ref`]): a pinned child of a
/// multi-repo policy proceeds against ITS pinned member; an unpinned child of a
/// multi-repo policy — or a pin the recipe no longer lists — is a terminal
/// validation error, never a silent repository #1 pick. Single-repo children
/// resolve exactly as before.
async fn resolve_recipe(
    ctx: &Context,
    backup: &Snapshot,
    namespace: &str,
) -> Result<(SnapshotPolicy, RepositoryRef, ResolvedRepository)> {
    let policy_ref = backup
        .spec
        .policy_ref
        .as_ref()
        .ok_or_else(|| Error::Invariant("produced Snapshot has no policyRef".into()))?;
    let cfg_ns = policy_ref.namespace.as_deref().unwrap_or(namespace);
    let cfg_api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), cfg_ns);
    let config = cfg_api.get_opt(&policy_ref.name).await?.ok_or_else(|| {
        Error::MissingDependency(format!("SnapshotPolicy {cfg_ns}/{}", policy_ref.name))
    })?;

    let repo_ref = kopiur_api::snapshot::effective_repository_ref(backup, &config.spec, cfg_ns)
        .map_err(|e| Error::Validation(e.to_string()))?;
    // Honor `repository.kind`: namespaced `Repository` (cross-ns via
    // `ref.namespace`, defaulting to the config's namespace) vs. cluster-scoped
    // `ClusterRepository` (`Api::all`). The discriminated kind is matched
    // exhaustively in the resolver (ADR §5.5).
    let repo = io::resolve_repository_ref(
        &ctx.client,
        &repo_ref,
        cfg_ns,
        ctx.operator_namespace.as_deref(),
    )
    .await?;
    Ok((config, repo_ref, repo))
}

/// Best-effort, **positive-only** securityContext check for a backup source PVC. Lists the
/// workload pods mounting `claim` and, when the mover is *provably* compatible (root, or an
/// exact UID match with the workload), records `SecurityContextCompatible=True`. It NEVER
/// writes `False` or emits an Event: a securityContext-only heuristic can't see file modes, so
/// a UID mismatch is not proof of unreadability (world-readable data reads fine). The certain
/// `False`+Event comes only from [`assess_completed_backup`] (kopia's own output); the
/// advisory negative lives in the admission warning. Never returns an error.
/// `listed_pods` is an already-fetched **unfiltered** namespace pod list to reuse instead of
/// listing again (the `pvcConsumer` resolver's). It MUST be unfiltered: `workload_identities`
/// needs every pod mounting the claim, and a narrowed writer set can flip a mismatch to a false
/// `Compatible`. `None` → list here, as every other caller does.
#[allow(clippy::too_many_arguments)]
async fn assess_backup_security_context(
    namespace: &str,
    backup: &Snapshot,
    claim: &str,
    source_read_only: bool,
    sc: &k8s_openapi::api::core::v1::SecurityContext,
    psc: Option<&k8s_openapi::api::core::v1::PodSecurityContext>,
    listed_pods: Option<&[Pod]>,
    ctx: &Context,
) {
    use kube::api::ListParams;

    let listed;
    let pods: &[Pod] = match listed_pods {
        Some(pods) => pods,
        None => {
            listed = match Api::<Pod>::namespaced(ctx.client.clone(), namespace)
                .list(&ListParams::default())
                .await
            {
                Ok(list) => list.items,
                // Best-effort: a transient list failure must never derail the backup.
                Err(e) => {
                    tracing::debug!(error = %e, %namespace, "securityContext compat: pod list failed; skipping");
                    return;
                }
            };
            &listed
        }
    };
    let mover = kopiur_api::secctx_compat::mover_identity(sc, psc);
    let identities = kopiur_api::secctx_compat::workload_identities(pods, claim);
    if let kopiur_api::secctx_compat::MoverReadCompat::Compatible { basis } =
        kopiur_api::secctx_compat::assess_read_compat(&mover, &identities, source_read_only)
    {
        // Name the basis and the actual UID: a bare "compatible" is what let the old
        // "by construction" claim hide the fact that nothing had been checked.
        let message = match basis {
            kopiur_api::secctx_compat::CompatBasis::RootMover => {
                "the mover runs as root (uid 0), so it can read the source regardless of ownership"
                    .to_string()
            }
            kopiur_api::secctx_compat::CompatBasis::ExactUidMatch => format!(
                "the mover's uid ({}) exactly matches every workload writing the source PVC `{claim}`",
                mover
                    .uid
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| "?".into()),
            ),
        };
        let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), namespace);
        set_security_context_compatible(&api, &backup.name_any(), backup, &message).await;
    }
    // Undecidable / likely-incompatible from securityContext alone → stay silent on the
    // reconcile path (no false alarms). The mover verifies it for real at runtime.
}

/// Report what `mover.inheritSecurityContextFrom` actually achieved, when it achieved
/// something other than plainly working. Warn-only — never blocks a run.
///
/// Each arm covers a way inheritance can quietly not do what the recipe implies:
///
/// - `Fallback` — no workload pod resolved; the explicit context stood in.
/// - `Inherited` but the workload pinned no identity — inheriting copied nothing usable and
///   the mover runs as its own image's UID. This is the misconfiguration behind the reported
///   `permission denied`-despite-`Compatible` bug.
/// - `Inherited` but an explicit `runAsUser` displaced it — correct by design (explicit wins),
///   but it makes inheritance a permanent no-op for that field, so it must not be silent.
///
/// **Backup-only.** A restore that inherits only an `fsGroup` is a *blessed* configuration
/// (`RestoreBasis::FsGroupMatch`) and maintenance has no source at all, so neither warrants an
/// "inheritance did nothing" warning.
async fn report_inherit_outcome(
    namespace: &str,
    backup: &Snapshot,
    mover_security: &io::ResolvedMoverSecurity,
    resolved: &kopiur_api::common::ResolvedMover,
    explicit: Option<&kopiur_api::common::MoverSpec>,
    ctx: &Context,
) {
    let Some(verdict) = inherit_verdict(&mover_security.outcome, resolved, explicit) else {
        return; // no inheritance requested — the condition never applies
    };

    // Re-read the Snapshot rather than using the copy this reconcile started with.
    // `assess_backup_security_context` runs just above and may have already patched
    // `SecurityContextCompatible` into status. A `conditions` patch REPLACES the whole array, so
    // computing it from the stale in-memory copy would silently erase that condition — two
    // writers in one reconcile, last one wins. (This is not hypothetical: it is what made the
    // e2e regression guard pass against a deliberately-reintroduced bug.)
    let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), namespace);
    let name = backup.name_any();
    let Some(live) = io::live_conditions_source(&api, &name, backup).await else {
        return; // deleted mid-reconcile
    };
    let existing = live
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    let conditions = io::upsert_condition(
        &existing,
        SECURITY_CONTEXT_INHERITED_CONDITION,
        verdict.ok,
        verdict.reason,
        &verdict.message,
        backup.meta().generation,
    );
    // Guard the Event behind an actual status transition. `publish_warning_event` has no dedup
    // of its own, and a Snapshot re-reconciles; without this the warning would re-fire on every
    // pass. `patch_status_if_changed` makes a repeat a true no-op.
    let current = serde_json::to_value(&live.status).ok();
    match io::patch_status_if_changed(
        &api,
        &name,
        current.as_ref(),
        serde_json::json!({ "conditions": conditions }),
    )
    .await
    {
        // Only a problem is worth an Event; the healthy arm is a silent confirmation.
        Ok(true) if !verdict.ok => {
            io::publish_warning_event(
                ctx,
                backup,
                verdict.reason,
                verdict.action,
                &verdict.message,
            )
            .await
        }
        Ok(_) => {}
        Err(e) => tracing::debug!(error = %e, "inherit outcome: condition patch failed"),
    }
}

/// The `SecurityContextInherited` verdict for one run. `None` ⇒ no inheritance was requested,
/// so the condition does not apply at all.
struct InheritVerdict {
    /// Condition status: `true` when inheritance did what the recipe implies.
    ok: bool,
    reason: &'static str,
    action: &'static str,
    message: String,
}

/// Decide what `inheritSecurityContextFrom` actually achieved. Pure — no IO — so every arm is
/// unit-testable without a cluster.
///
/// **Every requested-inherit path yields a verdict, including the healthy one.** An
/// early-return on the happy path would make the condition write-once-and-stick: a user who
/// fixed their recipe would keep a stale `InheritOverridden` forever, because nothing would
/// ever flip it back. This is why `MoverPermitted` carries an explicit "clear the stale False"
/// block — modelling the healthy state as a real verdict removes the need for one.
fn inherit_verdict(
    outcome: &io::InheritOutcome,
    resolved: &kopiur_api::common::ResolvedMover,
    explicit: Option<&kopiur_api::common::MoverSpec>,
) -> Option<InheritVerdict> {
    // What the mover ACTUALLY runs as, after every layer merged. `None` ⇒ no layer pinned a
    // UID, so it is the mover image's own.
    let effective = kopiur_api::common::effective_run_as_user(
        Some(&resolved.security_context),
        resolved.pod_security_context.as_ref(),
    );
    let identity = || match effective {
        Some(u) => format!("uid {u}"),
        None => format!(
            "its own image's uid {}",
            kopiur_api::common::MOVER_NONROOT_ID
        ),
    };

    match outcome {
        io::InheritOutcome::NotRequested => None,
        // Restore-only outcome (`inheritSecurityContextFrom.snapshot`): unreachable
        // from a backup — the variant is admission-rejected on SnapshotPolicy and the
        // backup reconciler passes no recorded source (the resolver errors before it
        // could ever produce this). Defensive: report no verdict rather than invent a
        // condition for a state that cannot arise here; the restore reconciler owns
        // the recorded-inherit reporting.
        io::InheritOutcome::InheritedFromSnapshot { snapshot, .. } => {
            tracing::debug!(
                %snapshot,
                "backup inherit_verdict saw the restore-only InheritedFromSnapshot outcome; \
                 ignoring (validators make this unreachable)"
            );
            None
        }
        io::InheritOutcome::Fallback { reason } => Some(InheritVerdict {
            ok: false,
            reason: INHERIT_FALLBACK_REASON,
            action: MATCH_WORKLOAD_SECURITY_CONTEXT_ACTION,
            message: format!(
                "the mover runs as {} from this recipe's explicit mover securityContext, \
                 not from the workload: {reason}. An explicit context that pins an \
                 identity is the deliberate fallback, so the run proceeded — but it is \
                 not tracking the workload. Scale the workload up or fix the selector to \
                 resume inheriting, or drop inheritSecurityContextFrom if the explicit \
                 context is the intent.",
                identity()
            ),
        }),
        // Inheriting copied nothing the mover would not have had anyway.
        io::InheritOutcome::Inherited {
            pod,
            container,
            pins_identity: false,
            ..
        } => Some(InheritVerdict {
            ok: false,
            reason: INHERIT_PINNED_NO_UID_REASON,
            action: PIN_WORKLOAD_RUN_AS_USER_ACTION,
            message: format!(
                "inheriting from pod `{pod}`{} copied nothing: the pod pins no runAsUser, \
                 and no runAsGroup/fsGroup/supplementalGroups beyond the mover's own \
                 defaults, so its identity lives in its container image, which Kopiur \
                 cannot read from the pod spec. The mover therefore runs as {} — an \
                 identity that did NOT come from the workload — and will likely fail to \
                 read the source with permission denied. Set runAsUser on the workload, \
                 or pin mover.securityContext.runAsUser in this recipe (it merges with, \
                 and overrides, inherited values).",
                container
                    .as_deref()
                    .map(|c| format!(" (container `{c}`)"))
                    .unwrap_or_default(),
                identity(),
            ),
        }),
        io::InheritOutcome::Inherited {
            pod,
            uid: Some(inherited_uid),
            ..
        } if effective != Some(*inherited_uid) => {
            // Only the recipe's explicit context sits above the inherited layer, and the
            // pair merge resolves the effective identity to the HIGHEST layer that pins
            // one — so a displaced inherited uid always means the recipe pinned the
            // winner. (The old "the repository's moverDefaults overrides inherited
            // values" branch died with the cross-dimension shadowing bug: a lower layer
            // can no longer displace an inherited identity, in either dimension.)
            // Intended — explicit wins by design — but it makes inheritance a permanent
            // no-op for that field, and the compat condition is positive-only so it
            // stays silent on exactly this shape. Name the exact field so the remedy is
            // a one-line edit.
            debug_assert_eq!(
                kopiur_api::common::effective_run_as_user(
                    explicit.and_then(|m| m.security_context.as_ref()),
                    explicit.and_then(|m| m.pod_security_context.as_ref()),
                ),
                effective,
                "a displaced inherited uid can only come from the recipe's explicit context"
            );
            let field = if explicit
                .and_then(|m| m.security_context.as_ref())
                .and_then(|s| s.run_as_user)
                == effective
            {
                "mover.securityContext.runAsUser"
            } else {
                "mover.podSecurityContext.runAsUser"
            };
            Some(InheritVerdict {
                ok: false,
                reason: INHERIT_OVERRIDDEN_REASON,
                action: MATCH_WORKLOAD_SECURITY_CONTEXT_ACTION,
                message: format!(
                    "the mover runs as {}, not the uid {inherited_uid} it inherited from \
                     pod `{pod}`: this recipe's explicit {field} overrides the inherited \
                     value — an explicit field always wins — so \
                     inheritSecurityContextFrom will not follow the workload if its uid \
                     changes. Remove {field} to track the workload, or drop \
                     inheritSecurityContextFrom to stop implying that it does.",
                    identity()
                ),
            })
        }
        // Inheritance resolved and stuck: either the workload's UID survived every layer, or it
        // contributed groups (no UID) which is legitimate — the mover reads group-readable data
        // through the group bit. Reported positively so a stale warning from a previous
        // reconcile is cleared rather than left to rot.
        io::InheritOutcome::Inherited { pod, container, .. } => Some(InheritVerdict {
            ok: true,
            reason: INHERIT_APPLIED_REASON,
            action: MATCH_WORKLOAD_SECURITY_CONTEXT_ACTION,
            message: format!(
                "the mover inherited its security context from pod `{pod}`{} and runs as {}",
                container
                    .as_deref()
                    .map(|c| format!(" (container `{c}`)"))
                    .unwrap_or_default(),
                identity(),
            ),
        }),
    }
}

/// Upsert `SecurityContextCompatible=True` on a Snapshot (idempotent, no Event — a positive
/// confirmation, never an alarm).
async fn set_security_context_compatible(
    api: &Api<Snapshot>,
    name: &str,
    backup: &Snapshot,
    message: &str,
) {
    // Not the first conditions writer in this reconcile — the "clear any stale
    // MoverPermitted=False" block runs above and may have already patched. Building `existing`
    // from the reconcile-start copy would revert that clear, flipping MoverPermitted back to
    // False on a run that IS permitted. See `io::live_conditions_source`.
    let Some(live) = io::live_conditions_source(api, name, backup).await else {
        return; // deleted mid-reconcile
    };
    let existing = live
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    let conditions = io::upsert_condition(
        &existing,
        SECURITY_CONTEXT_COMPATIBLE_CONDITION,
        true,
        SECURITY_CONTEXT_COMPATIBLE_REASON,
        message,
        backup.meta().generation,
    );
    let current = serde_json::to_value(&backup.status).ok();
    if let Err(e) = io::patch_status_if_changed(
        api,
        name,
        current.as_ref(),
        serde_json::json!({ "conditions": conditions }),
    )
    .await
    {
        tracing::debug!(error = %e, %name, "securityContext compat: condition patch failed");
    }
}

/// Post-run check (warn-only): a COMPLETED backup whose mover recorded excluded entries
/// (`status.stats.filesFailed > 0`) is *incomplete* — kopia skipped unreadable source files
/// under an ignore-file-errors policy. This is the certain runtime signal (kopia's own
/// output), so it sets `SecurityContextCompatible=False` + a once-per-transition Warning
/// Event with the actionable fix. Never returns an error.
async fn assess_completed_backup(
    ctx: &Context,
    api: &Api<Snapshot>,
    name: &str,
    backup: &Snapshot,
) {
    let failed = backup
        .status
        .as_ref()
        .and_then(|s| s.stats.as_ref())
        .and_then(|st| st.files_failed)
        .unwrap_or(0);
    if failed <= 0 {
        return;
    }
    let msg = format!(
        "the backup completed but {failed} source entr{} could not be read and were EXCLUDED \
         from the snapshot — it is INCOMPLETE. This is usually a UID/GID mismatch: match the \
         mover to the workload via mover.inheritSecurityContextFrom.pvcConsumer or a matching \
         runAsUser; otherwise fix the source file permissions",
        if failed == 1 { "y" } else { "ies" },
    );
    let existing = backup
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    let conditions = io::upsert_condition(
        &existing,
        SECURITY_CONTEXT_COMPATIBLE_CONDITION,
        false,
        SNAPSHOT_INCOMPLETE_REASON,
        &msg,
        backup.meta().generation,
    );
    let current = serde_json::to_value(&backup.status).ok();
    match io::patch_status_if_changed(
        api,
        name,
        current.as_ref(),
        serde_json::json!({ "conditions": conditions }),
    )
    .await
    {
        Ok(true) => {
            io::publish_warning_event(
                ctx,
                backup,
                SNAPSHOT_INCOMPLETE_REASON,
                crate::consts::MATCH_WORKLOAD_SECURITY_CONTEXT_ACTION,
                &msg,
            )
            .await;
        }
        Ok(false) => {}
        Err(e) => {
            tracing::debug!(error = %e, %name, "incomplete-backup condition patch failed");
        }
    }
}

/// Whether a Job reached a terminal state: `Some(true)` complete, `Some(false)`
/// failed, `None` still running.
pub(crate) fn job_terminal_state(job: &Job) -> Option<bool> {
    let status = job.status.as_ref()?;
    let conditions = status.conditions.as_ref();
    if let Some(conds) = conditions {
        for c in conds {
            if c.status == "True" {
                match c.type_.as_str() {
                    "Complete" => return Some(true),
                    "Failed" => return Some(false),
                    _ => {}
                }
            }
        }
    }
    // Fall back to counts when conditions aren't populated yet.
    if status.succeeded.unwrap_or(0) >= 1 {
        return Some(true);
    }
    None
}

/// Whether the staged-source teardown may proceed for a `Succeeded` Snapshot.
///
/// The mover stamps `phase: Succeeded` on the CR itself *before* its pod has
/// exited and *before* the Job controller marks the Job terminal. Reaping the
/// staged `-src` PVC + mover pod while the Job is still Active makes the Job
/// controller treat the vanished pod as a failure and spawn a replacement pod
/// that can never schedule (the `-src` PVC is already gone) — the Job then
/// lingers `Active` forever and trips `KubeJobNotCompleted` (#103). Only reap
/// once the Job is terminal (Complete/Failed) or already gone (TTL-reaped, or a
/// discovered Snapshot that never owned a Job).
fn staged_teardown_ready(job: Option<&Job>) -> bool {
    match job {
        None => true,
        Some(j) => job_terminal_state(j).is_some(),
    }
}

/// Whether this Snapshot's projected credential copies have already been reclaimed.
fn creds_reaped(backup: &Snapshot) -> bool {
    backup
        .status
        .as_ref()
        .and_then(|s| s.cleanup.as_ref())
        .and_then(|c| c.creds_reaped_at.as_ref())
        .is_some()
}

/// Reclaim the run's projected credential copies now that no mover Job can read
/// them, and stamp `status.cleanup.credsReapedAt`. Returns whether the reap is
/// settled — `false` asks the caller to come back (the Job is not terminal yet, or a
/// copy could not be removed).
///
/// A projected copy is only needed while a mover Job can load it via `envFrom` —
/// minutes. But it is owner-ref'd to this `Snapshot`, which owns the kopia snapshot
/// via a finalizer and so lives for the entire retention window. Without an explicit
/// reap, ownerRef GC leaves a live copy of the repository password and backend keys
/// sitting in the workload namespace for months (#240).
///
/// **The Job gate is load-bearing.** The mover PATCHes its terminal phase from *inside
/// the pod*, before that pod exits and before the Job controller marks the Job
/// terminal; with the default `backoffLimit` the Job can still schedule a replacement
/// pod that re-reads these very Secrets. Reaping under an Active Job would strand it in
/// `CreateContainerConfigError`. This is the same hazard, and the same gate, as the
/// staged-source teardown (#103).
///
/// **The stamp is load-bearing too.** A terminal Snapshot is re-reconciled every 10
/// minutes for its whole retention window, so an ungated probe would re-issue these
/// GETs per Snapshot, forever, only to rediscover there is nothing to do —
/// `pin_job_may_exist` exists in this file for exactly that reason. Stamped once, this
/// is a no-op thereafter (the `run_post_hooks_once` pattern).
async fn reap_backup_creds_once(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
) -> Result<bool> {
    if creds_reaped(backup) {
        return Ok(true);
    }
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    if !staged_teardown_ready(job_api.get_opt(name).await?.as_ref()) {
        return Ok(false);
    }
    let owner_uid = backup.uid().unwrap_or_default();
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    let outcome = io::reap_projection(
        &secrets,
        &io::CredsPrefix::snapshot_backup(name),
        &owner_uid,
        namespace,
        "run finished",
    )
    .await;
    if !outcome.settled {
        return Ok(false);
    }
    if outcome.deleted > 0 {
        ctx.metrics
            .inc_creds_secrets_reaped("terminal", outcome.deleted as u64);
    }
    // Stamped even when nothing was projected (the common same-namespace case): the
    // stamp records that cleanup RAN, which is what makes every later reconcile free.
    io::patch_status(
        api,
        name,
        serde_json::json!({
            "cleanup": { "credsReapedAt": chrono::Utc::now().to_rfc3339() }
        }),
    )
    .await?;
    Ok(true)
}

/// Backfill `status.resolved.credentialProjection` and/or `status.resolved.repository`
/// onto a `Snapshot` that ran before either pin existed.
///
/// The credential-projection pin is written at job-creation time (`resolved_run_status`),
/// so every run from that version on carries it; a `Snapshot` that already succeeded under
/// an older operator has none and would strand its finalizer the moment its `SnapshotPolicy`
/// is deleted (#255). The repository pin exists for a different consumer: the mass-deletion
/// breaker counts pending destructive deletions PER REPOSITORY straight from the `Snapshot`
/// store, keyed on this pin — a pre-#255 `Snapshot` without it would be invisible to that
/// per-repo count and fall into the conservative unpinned ("unknown") bucket forever. Every
/// `Snapshot` reconciles at least once on operator startup and lands here, so backfilling
/// from this branch converts the whole existing fleet before any recipe can disappear.
///
/// Idempotent and independently gated per pin (`needs_projection`/`needs_repository`): a
/// `Snapshot` needing only one is patched with only that key, and the guard for each is false
/// forever after its first backfill. Cost: at most one `SnapshotPolicy` GET per legacy
/// `Snapshot`, once. The one case that re-GETs per steady-state pass is a legacy `Snapshot`
/// whose recipe is ALREADY gone — a shrinking, already-stuck set — and even then the
/// repository pin is deliberately left unset (tolerated: the conservative unpinned bucket
/// covers it) rather than blocking on a recipe that will never come back.
async fn backfill_projection_pin(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    name: &str,
) -> Result<()> {
    let has_snapshot = backup
        .status
        .as_ref()
        .and_then(|s| s.snapshot.as_ref())
        .is_some();
    // Credential projection only matters once there is a kopia snapshot for a
    // finalizer to delete; the repository pin has no such gate — it matters
    // the moment the Snapshot exists, so the breaker can attribute it.
    let needs_projection = pinned_projection(backup).is_none() && has_snapshot;
    let needs_repository = plan::needs_repository_backfill(backup);
    if !needs_projection && !needs_repository {
        return Ok(());
    }
    let Some(policy_ref) = backup.spec.policy_ref.as_ref() else {
        return Ok(());
    };
    let cfg_ns = policy_ref.namespace.as_deref().unwrap_or(namespace);
    let cfg_api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), cfg_ns);
    let Some(config) = cfg_api.get_opt(&policy_ref.name).await? else {
        return Ok(());
    };
    // The row's effective repository (the spec pin for a multi-repo child).
    // A pre-feature row of a now-multi-repo policy has NO pin, so its
    // repository is unknowable — the repository half is SKIPPED (never
    // guessed; `backfill_patch_body` documents the contract). The projection
    // half still backfills.
    let repo_ref =
        match kopiur_api::snapshot::effective_repository_ref(backup, &config.spec, cfg_ns) {
            Ok(r) => Some(r),
            Err(kopiur_api::error::ValidationError::MultiRepoSnapshotUnpinned { .. }) => None,
            Err(other) => return Err(Error::Validation(other.to_string())),
        };
    let Some(body) = plan::backfill_patch_body(
        &config,
        namespace,
        needs_projection,
        needs_repository,
        repo_ref.as_ref(),
    ) else {
        return Ok(());
    };
    io::patch_status(api, name, body).await?;
    tracing::info!(
        backup = %name,
        needs_projection,
        needs_repository,
        "backfilled resolved pin(s) onto a Snapshot that predates them"
    );
    Ok(())
}

/// What the running-Job staged-PVC bind watchdog observed.
enum StagedPvcWatch {
    /// No staged source, or the staged PVC is Bound/absent — proceed to the
    /// normal wedged-pod check.
    Clear,
    /// The staged PVC is still Pending within the pinned staging budget — the
    /// mover pod cannot start yet and is NOT wedged; just requeue.
    Provisioning,
    /// The staged PVC is unbound past the pinned budget (or `Lost`) — terminal,
    /// with the same reason/message family as the pre-Job bind gate.
    Expired {
        reason: &'static str,
        message: String,
    },
}

/// IO half of the staged-PVC bind watchdog: read the staged PVC named in
/// `status.staged` and run it through the same pure [`io::pvc_bind_outcome`]
/// decision the pre-Job gate uses — the PVC's own `status.phase` is ground truth,
/// version-independent, where scheduler `Unschedulable`/`SchedulerError` message
/// text is not. The budget is `status.staged.stagingTimeoutSeconds` (pinned at
/// stamp time — never re-resolved from a policy that may have been edited or
/// deleted mid-run); a legacy stamp without the field gets the default budget
/// rather than an indefinite wait.
async fn staged_pvc_watchdog(
    backup: &Snapshot,
    ctx: &Context,
    namespace: &str,
) -> Result<StagedPvcWatch> {
    let Some(staged) = backup.status.as_ref().and_then(|s| s.staged.as_ref()) else {
        return Ok(StagedPvcWatch::Clear);
    };
    let Some(pvc_name) = staged.pvc_name.as_deref() else {
        return Ok(StagedPvcWatch::Clear);
    };
    let pvc_api: Api<k8s_openapi::api::core::v1::PersistentVolumeClaim> =
        Api::namespaced(ctx.client.clone(), namespace);
    // A missing staged PVC is not this watchdog's problem (already reaped, or a
    // race with cleanup) — the wedge check / terminal paths handle the rest.
    let Some(pvc) = pvc_api.get_opt(pvc_name).await? else {
        return Ok(StagedPvcWatch::Clear);
    };
    let timeout = staged_watchdog_budget(staged.staging_timeout_seconds);
    Ok(
        match io::pvc_bind_outcome(
            &io::staged_pvc_observation(&pvc),
            namespace,
            pvc_name,
            staged.storage_class_name.as_deref(),
            timeout,
            chrono::Utc::now(),
        ) {
            io::PvcBindWait::Bound => StagedPvcWatch::Clear,
            io::PvcBindWait::Waiting(_) => StagedPvcWatch::Provisioning,
            io::PvcBindWait::Failed { reason, message } => {
                StagedPvcWatch::Expired { reason, message }
            }
        },
    )
}

/// The watchdog's bind budget from the pinned `status.staged.stagingTimeoutSeconds`:
/// `0` = the user's explicit "wait indefinitely"; absent (a stamp from before the
/// field existed) = the default staging budget, never an accidental infinite wait.
fn staged_watchdog_budget(pinned_seconds: Option<i64>) -> Option<std::time::Duration> {
    match pinned_seconds {
        Some(0) => None,
        Some(s) if s > 0 => Some(std::time::Duration::from_secs(s as u64)),
        _ => Some(crate::consts::DEFAULT_STAGING_TIMEOUT),
    }
}

/// `error_policy` for the `Snapshot` controller.
pub fn error_policy(backup: Arc<Snapshot>, err: &Error, ctx: Arc<Context>) -> Action {
    error_policy_for("Snapshot", backup.as_ref(), err, &ctx)
}
