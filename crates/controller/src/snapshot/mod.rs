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
//! [`plan_deletion`] is a pure function over `(DeletionPolicy, annotations)`
//! returning a [`DeletionPlan`]. It is the single most important thing to get
//! right and is exhaustively unit-tested — the `match` has **no** `_ =>` arm, so
//! a new `DeletionPolicy` variant cannot compile until handled (SKILL thesis).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::DeleteParams;
use kube::runtime::controller::Action;
use kube::runtime::events::{Event, EventType};
use kube::{Api, Resource, ResourceExt};

use kopiur_api::backend::Backend;
use kopiur_api::common::RepositoryRef;
use kopiur_api::snapshot::SnapshotPhase;
use kopiur_api::{DeletionPolicy, Origin, Snapshot, SnapshotPolicy};
use kopiur_mover::workspec::{
    MoverOptions, MoverWorkSpec, Operation, SnapshotDeleteOp, SnapshotPinOp, TargetRef,
};

use crate::config;
use crate::consts::{
    ALLOW_PRIVILEGED_MOVER_ACTION, API_VERSION, CONFIG_LABEL, CREDENTIALS_AVAILABLE_CONDITION,
    CREDENTIALS_PROJECTED_REASON, FIX_HOOK_ACTION, FIX_SNAPSHOT_STACK_ACTION,
    HOOKS_SUCCEEDED_CONDITION, MISSING_CREDENTIALS_REASON, MOVER_PERMITTED_CONDITION, ORIGIN_LABEL,
    PRIVILEGED_MOVER_NOT_PERMITTED_REASON, SECURITY_CONTEXT_COMPATIBLE_CONDITION,
    SECURITY_CONTEXT_COMPATIBLE_REASON, SNAPSHOT_CLEANUP_FINALIZER, SNAPSHOT_INCOMPLETE_REASON,
    SOURCE_STAGED_CONDITION, SOURCE_STAGED_REASON, STAGING_WAITING_REASON,
};
use crate::context::Context;
use crate::error::{Error, Result, error_policy_for};
use crate::io::{self, ResolvedRepository};
use crate::jobs::{self, JobLimits, MoverJobInputs, VolumeMountSpec};

mod build;
mod plan;

pub(crate) use build::*;
pub use plan::*;

#[cfg(test)]
mod tests;
/// Reconcile a `Snapshot`.
///
/// IO is intentionally thin here: the decision logic ([`plan_deletion`],
/// [`effective_deletion_policy`], the job builders in [`crate::jobs`]) is pure
/// and unit-tested; this function wires those decisions to the cluster.
#[tracing::instrument(skip(backup, ctx), fields(kind = "Snapshot", namespace = %backup.namespace().unwrap_or_default(), name = %backup.name_any()))]
pub async fn reconcile(backup: Arc<Snapshot>, ctx: Arc<Context>) -> Result<Action> {
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

async fn reconcile_inner(backup: &Snapshot, ctx: &Context) -> Result<Action> {
    let origin = resolve_origin(backup);
    let policy = effective_deletion_policy(backup.spec.deletion_policy, origin);
    let namespace = backup
        .namespace()
        .ok_or_else(|| Error::Invariant("Snapshot has no namespace".into()))?;
    let name = backup.name_any();
    let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), &namespace);

    if backup.metadata.deletion_timestamp.is_some() {
        return handle_deletion(backup, ctx, &api, &namespace, &name, policy).await;
    }

    // Discovered backups are catalog rows, not runs: never spawn a Job. Pin the
    // Discovered phase (with kstatus Ready, ADR-0005 §2) if unset and stop.
    if origin == Origin::Discovered {
        if backup.status.as_ref().and_then(|s| s.phase) != Some(SnapshotPhase::Discovered) {
            let mut status = snapshot_ready_status(
                backup,
                SnapshotPhase::Discovered,
                "Discovered",
                "catalog-materialized snapshot",
            );
            status["origin"] = serde_json::json!("discovered");
            io::patch_status(&api, &name, status).await?;
        }
        return Ok(Action::requeue(Duration::from_secs(600)));
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
    match run_decision(backup.status.as_ref().and_then(|s| s.phase)) {
        RunDecision::Run => {}
        RunDecision::SucceededSteadyState => {
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
                io::patch_status(
                    &api,
                    &name,
                    snapshot_ready_status(
                        backup,
                        SnapshotPhase::Succeeded,
                        "SnapshotCreated",
                        "the kopia snapshot was created successfully",
                    ),
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
                ctx.metrics
                    .inc_snapshot_completed("succeeded", &namespace, backup_policy(backup));
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
                io::cleanup_staged_source(&ctx.client, &namespace, &name).await?;
            }
            // §13(c): spec.pin stays live after the mover Job is gone.
            return reconcile_pin(backup, ctx, &api, &namespace, &name).await;
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
                let current = serde_json::to_value(&backup.status).ok();
                let wrote = io::patch_status_if_changed(
                    &api,
                    &name,
                    current.as_ref(),
                    snapshot_ready_status(
                        backup,
                        SnapshotPhase::Failed,
                        "SnapshotFailed",
                        "the backup failed; see status.failure and the mover Job/pod logs",
                    ),
                )
                .await?;
                if wrote {
                    ctx.metrics
                        .inc_snapshot_completed("failed", &namespace, backup_policy(backup));
                }
            }
            return Ok(Action::await_change());
        }
        RunDecision::Wait => return Ok(Action::await_change()),
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
                if backup.status.as_ref().and_then(|s| s.phase) != Some(SnapshotPhase::Succeeded) {
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
                    io::cleanup_staged_source(&ctx.client, &namespace, &name).await?;
                }
                // §13(c): reconcile kopia-side pin state with spec.pin once the
                // snapshot exists. A no-op when already in the desired state.
                return reconcile_pin(backup, ctx, &api, &namespace, &name).await;
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
                if backup.status.as_ref().and_then(|s| s.phase) != Some(SnapshotPhase::Failed) {
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
                    if let Some(repo_ref) = backup
                        .status
                        .as_ref()
                        .and_then(|s| s.resolved.as_ref())
                        .and_then(|r| r.repository.as_ref())
                        && let Err(e) = io::request_repository_reverify(
                            &ctx.client,
                            repo_ref,
                            &namespace,
                            chrono::Utc::now(),
                        )
                        .await
                    {
                        tracing::debug!(backup = %name, error = %e, "repository reverify nudge failed (ignored)");
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
                    io::cleanup_staged_source(&ctx.client, &namespace, &name).await?;
                }
                return Ok(Action::requeue(Duration::from_secs(120)));
            }
            None => {
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
                    // kubelet stops retrying immediately — don't wait for TTL/ownerRef GC.
                    let _ = job_api.delete(&name, &DeleteParams::background()).await;
                    if backup
                        .status
                        .as_ref()
                        .and_then(|s| s.staged.as_ref())
                        .is_some()
                    {
                        io::cleanup_staged_source(&ctx.client, &namespace, &name).await?;
                    }
                    return Ok(Action::requeue(Duration::from_secs(120)));
                }
                // Job exists but is still running/starting; mark Running and wait.
                if backup.status.as_ref().and_then(|s| s.phase) != Some(SnapshotPhase::Running) {
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
    let (config, repo) = resolve_recipe(ctx, backup, &namespace).await?;

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
        let conditions = io::upsert_condition(
            &conds,
            crate::consts::REPOSITORY_WRITABLE_CONDITION,
            false,
            crate::consts::REPOSITORY_READ_ONLY_REASON,
            &readonly_backup_message(&config.spec.repository.name),
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
                        note: Some(readonly_backup_message(&config.spec.repository.name)),
                        action: "RefuseBackupReadOnlyRepository".into(),
                        secondary: None,
                    },
                    &io::event_ref(backup),
                )
                .await;
            tracing::warn!(backup = %name, repository = %config.spec.repository.name, "refusing backup: repository is ReadOnly");
        }
        return Ok(Action::await_change());
    }

    // Don't launch a mover Job against an unreachable repository (`phase != Ready`):
    // the pod would only fail on `kopia repository connect`. Hold the Snapshot in
    // `Pending` and requeue until the repository's own reconcile marks it `Ready`.
    // Same gate Maintenance, `SnapshotPolicy`, and `RepositoryReplication` apply.
    // A cheap single GET — independent of preflight, so it's evaluated FIRST and the
    // repository-not-ready reason is always surfaced before any preflight machinery.
    if !io::repository_ready(&ctx.client, &config.spec.repository, &namespace).await? {
        let current = serde_json::to_value(&backup.status).ok();
        io::patch_status_if_changed(
            &api,
            &name,
            current.as_ref(),
            snapshot_ready_status(
                backup,
                SnapshotPhase::Pending,
                crate::consts::REPOSITORY_NOT_READY_REASON,
                &repository_not_ready_message(&config.spec.repository.name),
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
        && should_run_preflight(backup.status.as_ref().and_then(|s| s.phase))
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
            &config.spec.repository,
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

    let (work_spec, mut source_volume, repo_volume, _) =
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
            let conditions = io::upsert_condition(
                &existing,
                CREDENTIALS_AVAILABLE_CONDITION,
                false,
                crate::consts::MISSING_SERVICE_ACCOUNT_REASON,
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
    let (effective_sc, effective_pod_sc) = io::resolve_mover_security_contexts(
        &ctx.client,
        &namespace,
        config.spec.mover.as_ref(),
        source_pvc,
    )
    .await?;
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
        let conditions = io::upsert_condition(
            &existing,
            MOVER_PERMITTED_CONDITION,
            false,
            PRIVILEGED_MOVER_NOT_PERMITTED_REASON,
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

    // SecurityContext-compatibility (positive-only, best-effort): confirm `True` when the
    // mover provably can read the source. `pvcConsumer` matches the source PVC's consumer by
    // construction — set `True` directly without a second namespace pod LIST (the inherit
    // resolver already listed). Otherwise assess against the live consumers. Never writes
    // `False`/Event here — that comes certainly from `assess_completed_backup`.
    if let Some(claim) = source_pvc {
        let used_pvc_consumer = matches!(
            config
                .spec
                .mover
                .as_ref()
                .and_then(|m| m.inherit_security_context_from.as_ref()),
            Some(kopiur_api::common::InheritSecurityContextFrom::PvcConsumer(
                _
            ))
        );
        if used_pvc_consumer {
            set_security_context_compatible(
                &api,
                &name,
                backup,
                "the mover inherited the source PVC consumer's securityContext (pvcConsumer), so \
                 its UID/GID matches the workload by construction",
            )
            .await;
        } else {
            assess_backup_security_context(
                &namespace,
                backup,
                claim,
                &resolved_mover.security_context,
                resolved_mover.pod_security_context.as_ref(),
                ctx,
            )
            .await;
        }
    }

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
        &name,
        &owner,
        &repo,
        config
            .spec
            .credential_projection
            .as_ref()
            .is_some_and(|p| p.enabled),
        io::repo_kind_str(config.spec.repository.kind),
        &config.spec.repository.name,
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
            let conditions = io::upsert_condition(
                &existing,
                CREDENTIALS_AVAILABLE_CONDITION,
                false,
                MISSING_CREDENTIALS_REASON,
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
    let creds_secrets = creds.names;

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
                        "pvcName": staged.pvc_name,
                        "ready": true,
                    },
                }),
            )
            .await?;
            Some(staged.pvc_name)
        }
        io::StagingOutcome::Waiting(msg) => {
            // The VolumeSnapshot isn't readyToUse yet — a normal, transient wait.
            // The message may carry the VolumeSnapshot's (possibly transient)
            // `status.error` as diagnostic context; that is NOT a failure — see
            // `StagingOutcome::Failed` for the deadline that is (issue #198).
            let existing = backup
                .status
                .as_ref()
                .map(|s| s.conditions.clone())
                .unwrap_or_default();
            let conditions = io::upsert_condition(
                &existing,
                SOURCE_STAGED_CONDITION,
                false,
                STAGING_WAITING_REASON,
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
            let status = snapshot_ready_status_with_condition(
                backup,
                SnapshotPhase::Failed,
                reason,
                &message,
                SOURCE_STAGED_CONDITION,
                false,
            );
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
    // across individual Snapshot runs (ADR §3.1).
    let cache_volume = crate::cache::resolve_cache_volume(
        &ctx.client,
        &namespace,
        io::owner_ref_for(&config, "SnapshotPolicy")?,
        &format!("kopiur-cache-{}", config.name_any()),
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
            let decision = io::resolve_source_colocation(
                &ctx.client,
                &namespace,
                &claim,
                resolved_mover.source_colocation,
            )
            .await?;
            io::apply_colocation(
                decision,
                resolved_mover.affinity.clone(),
                resolved_mover.tolerations.clone(),
            )?
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
        annotations: Default::default(),
        cache_volume,
        scratch_volume: None,
        readiness_exec: None,
    };
    let cm = jobs::build_config_map(&inputs)?;
    let job = jobs::build_job(&inputs);
    io::apply_mover_objects(&ctx.client, &namespace, &name, &cm, &job).await?;

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
            "resolved": resolved_run_status(&config, &namespace, &work_spec),
        }),
    )
    .await?;
    tracing::info!(backup = %name, "created mover Job for backup");

    Ok(Action::requeue(Duration::from_secs(30)))
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

/// Execute the deletion plan (the tested [`plan_deletion`] decision) against the
/// cluster, then remove the finalizer when cleanup completes.
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
        io::cleanup_staged_source(&ctx.client, namespace, name).await?;
    }

    let base_plan = plan_deletion(policy, backup.annotations());

    // Namespace-deletion cascade (ADR-0005 §5): if the owning namespace is being torn
    // down, the repository's `onNamespaceDelete` decides. Default `Orphan` keeps
    // off-site history (a `kubectl delete ns` must not be a data-loss event); only an
    // explicit `Delete` cascades to the per-Snapshot plan. On a transient read error
    // fall back to the per-Snapshot plan: a single delete still works, and the
    // namespace-cascade case re-evaluates on the next pass once the read succeeds.
    let ns_terminating = io::namespace_is_terminating(&ctx.client, namespace)
        .await
        .unwrap_or(false);

    // Resolve the repository once for the whole deletion path — preferring the
    // ref pinned into `status.resolved.repository` over the live recipe, which
    // the namespace reaper usually deletes (no finalizer) before this finalizer
    // runs. Only needed when the cascade policy must be consulted or a delete
    // Job must be built; Retain/Orphan of a lone CR stays IO-free.
    let resolved = if ns_terminating || matches!(base_plan, DeletionPlan::DeleteSnapshot) {
        Some(resolve_repo_for_deletion(ctx, backup, namespace).await)
    } else {
        None
    };

    let plan = if ns_terminating {
        match resolved.as_ref() {
            Some(Ok((_, repo))) => namespace_delete_plan(repo.on_namespace_delete, true, base_plan),
            // The repository itself can no longer be resolved (already gone):
            // fail safe to Orphan — never guess Delete with history at stake.
            _ => DeletionPlan::OrphanSnapshot,
        }
    } else {
        base_plan
    };
    tracing::info!(?plan, backup = %name, ns_terminating, "executing backup deletion plan");

    match plan {
        DeletionPlan::DeleteSnapshot => {
            let snapshot_id = backup
                .status
                .as_ref()
                .and_then(|s| s.snapshot.as_ref())
                .map(|s| s.kopia_snapshot_id.clone());
            match snapshot_id {
                // No snapshot was ever recorded: nothing to delete in the repo.
                None => {
                    io::remove_finalizer(api, backup, SNAPSHOT_CLEANUP_FINALIZER).await?;
                    Ok(Action::await_change())
                }
                Some(id) => {
                    let (repo_ref, repo) = match resolved {
                        Some(r) => r?,
                        // Unreachable by construction (plan=Delete implies the
                        // resolution above ran); resolve again rather than panic.
                        None => resolve_repo_for_deletion(ctx, backup, namespace).await?,
                    };
                    match delete_job_placement(
                        ns_terminating,
                        namespace,
                        repo.repo_namespace.as_deref(),
                        ctx.operator_namespace.as_deref(),
                    ) {
                        DeleteJobPlacement::RunIn(job_ns) => {
                            delete_snapshot_via_job(
                                backup, ctx, api, namespace, &job_ns, name, &id, &repo_ref, &repo,
                            )
                            .await
                        }
                        DeleteJobPlacement::OrphanFallback { reason } => {
                            orphan_snapshot(backup, ctx, api, namespace, name, &reason).await
                        }
                    }
                }
            }
        }
        DeletionPlan::RetainSnapshot => {
            io::remove_finalizer(api, backup, SNAPSHOT_CLEANUP_FINALIZER).await?;
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
    }
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
        let repo = io::resolve_repository_ref(&ctx.client, pinned, namespace).await?;
        return Ok((pinned.clone(), repo));
    }
    let (config, repo) = resolve_recipe(ctx, backup, namespace).await?;
    let config_ns = config.namespace().unwrap_or_else(|| namespace.to_string());
    Ok((
        pinned_repository_ref(&config.spec.repository, &config_ns),
        repo,
    ))
}

/// Drive a SnapshotDelete mover Job for the deletion path. Creates the Job if
/// absent; on terminal success removes the finalizer; on failure records a
/// Deleting phase, bumps the failure metric, and requeues.
///
/// `job_ns` is where the Job runs (decided by [`delete_job_placement`]); it is
/// the `Snapshot`'s own namespace except during the namespace-deletion cascade,
/// where creating anything in the terminating namespace is rejected by the API
/// server. Everything the Job needs is preferred from values pinned at run time
/// (`status.snapshot.identity`, the resolved `repo`), so it works after the
/// recipe is gone.
#[allow(clippy::too_many_arguments)]
async fn delete_snapshot_via_job(
    backup: &Snapshot,
    ctx: &Context,
    api: &Api<Snapshot>,
    namespace: &str,
    job_ns: &str,
    name: &str,
    snapshot_id: &str,
    repo_ref: &RepositoryRef,
    repo: &ResolvedRepository,
) -> Result<Action> {
    let cross_namespace = job_ns != namespace;
    // A cross-namespace Job embeds the source namespace: two namespaces can each
    // hold a `Snapshot` of the same name, and both cascades may target `job_ns`.
    let job_name = if cross_namespace {
        capped_name(&format!("{namespace}-{name}-delete"))
    } else {
        format!("{name}-delete")
    };
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), job_ns);

    if let Some(job) = job_api.get_opt(&job_name).await? {
        match job_terminal_state(&job) {
            Some(true) => {
                io::remove_finalizer(api, backup, SNAPSHOT_CLEANUP_FINALIZER).await?;
                // A cross-namespace Job is not GC'd with the Snapshot (its owner
                // is the longer-lived repository CR) — reap it and its work-spec
                // ConfigMap now; best-effort, the owner ref is the backstop.
                if cross_namespace {
                    let _ = job_api.delete(&job_name, &DeleteParams::background()).await;
                    let cm_api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), job_ns);
                    let _ = cm_api.delete(&job_name, &DeleteParams::default()).await;
                }
                tracing::info!(backup = %name, %snapshot_id, "snapshot deleted; finalizer removed");
                return Ok(Action::await_change());
            }
            Some(false) => {
                ctx.metrics.inc_snapshot_deletion_failure(namespace);
                io::patch_status(api, name, serde_json::json!({ "phase": "Deleting" })).await?;
                tracing::warn!(backup = %name, "snapshot delete Job failed; backing off");
                return Ok(Action::requeue(Duration::from_secs(60)));
            }
            None => return Ok(Action::requeue(Duration::from_secs(15))),
        }
    }

    // Create the SnapshotDelete Job. The recipe is OPTIONAL here: identity is
    // preferred from the value pinned at success time, and the repository was
    // already resolved by the caller — so deletion (including the namespace
    // cascade) still works when the SnapshotPolicy has already been deleted.
    let config = resolve_recipe(ctx, backup, namespace)
        .await
        .ok()
        .map(|(c, _)| c);
    let identity = match pinned_mover_identity(backup) {
        Some(identity) => identity,
        None => {
            let config = config.as_ref().ok_or_else(|| {
                Error::MissingDependency(format!(
                    "Snapshot {namespace}/{name} has no pinned identity \
                     (status.snapshot.identity) and its SnapshotPolicy is gone; cannot build \
                     the snapshot-delete Job — re-create the SnapshotPolicy, or use the \
                     skip-snapshot-cleanup annotation to release the CR without deleting"
                ))
            })?;
            resolve_identity_for(config, namespace, repo.identity_defaults.as_ref())?
        }
    };
    // In the Snapshot's own namespace the Job is owned by (and GC'd with) the
    // Snapshot. A cross-namespace cascade Job cannot be (cross-namespace owner
    // references are invalid) — the repository CR, which outlives the namespace,
    // owns it instead.
    let owner = if cross_namespace {
        repo.owner_ref.clone()
    } else {
        io::owner_ref_for(backup, "Snapshot")?
    };
    // Resolve (and, when `spec.credentialProjection` is enabled, project) the mover's
    // credential Secret(s) into the Job's namespace before building the Job. Errors
    // propagate as MissingDependency (Transient) — this is the delete path, so we
    // requeue rather than surface a CredentialsAvailable condition. The cascade
    // placements need no projection by construction (the Job runs where the
    // repository's canonical Secret lives), so a gone recipe defaulting the
    // consumer opt-in to `false` is correct.
    let creds = io::resolve_mover_creds_for(
        &ctx.client,
        job_ns,
        &job_name,
        &owner,
        repo,
        config
            .as_ref()
            .and_then(|c| c.spec.credential_projection.as_ref())
            .is_some_and(|p| p.enabled),
        io::repo_kind_str(repo_ref.kind),
        &repo_ref.name,
    )
    .await?;
    if creds.projected > 0 {
        ctx.metrics.inc_secrets_projected(job_ns, creds.projected);
    }
    let creds_secrets = creds.names;
    let work_spec = MoverWorkSpec {
        version: 1,
        operation: Operation::SnapshotDelete(SnapshotDeleteOp {
            snapshot_id: snapshot_id.to_string(),
            // The recorded id can be stale (kopia rewrites the manifest id on
            // pin); the mover re-resolves the live id by these anchors so a
            // pinned snapshot isn't silently orphaned under deletionPolicy: Delete.
            anchor: snapshot_anchor(backup),
        }),
        identity,
        repository: repository_connect(repo)?,
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            kind: "Snapshot".to_string(),
            name: name.to_string(),
            namespace: namespace.to_string(),
        },
        hook_plan: Default::default(),
        options: MoverOptions::default(),
        // A one-shot finalizer delete: kopia's default cache is fine.
        cache: Default::default(),
        throttle: Default::default(),
    };

    // Recipe labels when it still exists; otherwise reconstruct from the
    // Snapshot itself (origin + the policyRef name it was produced from).
    let mut labels = match config.as_ref() {
        Some(config) => run_labels(config, resolve_origin(backup)),
        None => {
            let mut labels = BTreeMap::new();
            labels.insert(
                ORIGIN_LABEL.to_string(),
                origin_str(resolve_origin(backup)).to_string(),
            );
            if let Some(policy_ref) = backup.spec.policy_ref.as_ref() {
                labels.insert(CONFIG_LABEL.to_string(), policy_ref.name.clone());
            }
            labels
        }
    };
    labels.insert(
        "kopiur.home-operations.com/op".to_string(),
        "snapshot-delete".to_string(),
    );
    let repo_volume =
        io::filesystem_repo_mount_source(&repo.backend).map(|source| VolumeMountSpec {
            source,
            mount_path: io::filesystem_repo_path(&repo.backend).unwrap_or_default(),
            read_only: false,
        });
    // The finalizer delete-Job has no recipe `mover`, but still inherits the
    // repository's moverDefaults (security context, placement) so it can reach a
    // filesystem/NFS repo on a non-65532-owned directory (ADR-0004 §1).
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
    // Resolve the delete Job's run identity in its namespace before launching
    // (its credential Secret(s) were resolved/projected above).
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
        name: &job_name,
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
        annotations: Default::default(),
        // A one-shot finalizer delete: an ephemeral emptyDir cache is fine.
        cache_volume: Default::default(),
        scratch_volume: None,
        readiness_exec: None,
    };
    let cm = jobs::build_config_map(&inputs)?;
    let job = jobs::build_job(&inputs);
    io::apply_mover_objects(&ctx.client, job_ns, &job_name, &cm, &job).await?;
    io::patch_status(api, name, serde_json::json!({ "phase": "Deleting" })).await?;
    tracing::info!(backup = %name, %snapshot_id, job_namespace = %job_ns, "created SnapshotDelete Job");
    Ok(Action::requeue(Duration::from_secs(15)))
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
) -> Result<Action> {
    let desired = backup.spec.pin;
    let observed = backup.status.as_ref().and_then(|s| s.pinned);
    let steady = Action::requeue(Duration::from_secs(600));
    let action = pin_decision(desired, observed);
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

    let job_name = format!("{name}-pin");
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    if let Some(job) = job_api.get_opt(&job_name).await? {
        match job_terminal_state(&job) {
            Some(true) => {
                // Record the new observed pin state so the next reconcile is a NoOp.
                io::patch_status(api, name, serde_json::json!({ "pinned": desired })).await?;
                tracing::info!(backup = %name, %snapshot_id, pin = desired, "snapshot pin reconciled");
                return Ok(steady);
            }
            Some(false) => {
                tracing::warn!(backup = %name, "snapshot pin Job failed; backing off");
                return Ok(Action::requeue(Duration::from_secs(120)));
            }
            None => return Ok(Action::requeue(Duration::from_secs(15))),
        }
    }

    // Create the SnapshotPin Job (mirrors the SnapshotDelete one-shot path).
    let (config, repo) = resolve_recipe(ctx, backup, namespace).await?;
    let identity = resolve_identity_for(&config, namespace, repo.identity_defaults.as_ref())?;
    let owner = io::owner_ref_for(backup, "Snapshot")?;
    let creds = io::resolve_mover_creds_for(
        &ctx.client,
        namespace,
        &job_name,
        &owner,
        &repo,
        config
            .spec
            .credential_projection
            .as_ref()
            .is_some_and(|p| p.enabled),
        io::repo_kind_str(config.spec.repository.kind),
        &config.spec.repository.name,
    )
    .await?;
    if creds.projected > 0 {
        ctx.metrics
            .inc_secrets_projected(namespace, creds.projected);
    }
    let creds_secrets = creds.names;
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
    let mut labels = run_labels(&config, resolve_origin(backup));
    labels.insert(
        "kopiur.home-operations.com/op".to_string(),
        "snapshot-pin".to_string(),
    );
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
        annotations: Default::default(),
        cache_volume: Default::default(),
        scratch_volume: None,
        readiness_exec: None,
    };
    let cm = jobs::build_config_map(&inputs)?;
    let job = jobs::build_job(&inputs);
    io::apply_mover_objects(&ctx.client, namespace, &job_name, &cm, &job).await?;
    tracing::info!(backup = %name, %snapshot_id, ?action, "created SnapshotPin Job");
    Ok(Action::requeue(Duration::from_secs(15)))
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
    let (config, repo) = resolve_recipe(ctx, backup, namespace).await?;
    let identity = resolve_identity_for(&config, namespace, repo.identity_defaults.as_ref())?;
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
            // Match the snapshot by its source path (the path we snapshotted),
            // newest first. The pod's recorded user/host differ from our
            // resolved identity (a documented mover-identity follow-up), so we
            // key on the source path which IS authoritative.
            let mut list = client.snapshot_list(None).await?;
            list.sort_by_key(|e| std::cmp::Reverse(e.end_time));
            let matched = list
                .into_iter()
                .find(|e| e.source.path == identity.source_path);
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

/// Resolve a `Snapshot`'s referenced `SnapshotPolicy` and that config's
/// `Repository`. Cluster references and non-filesystem backends still resolve
/// here; backend-specific behavior is decided downstream.
async fn resolve_recipe(
    ctx: &Context,
    backup: &Snapshot,
    namespace: &str,
) -> Result<(SnapshotPolicy, ResolvedRepository)> {
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

    // Honor `repository.kind`: namespaced `Repository` (cross-ns via
    // `ref.namespace`, defaulting to the config's namespace) vs. cluster-scoped
    // `ClusterRepository` (`Api::all`). The discriminated kind is matched
    // exhaustively in the resolver (ADR §5.5).
    let repo = io::resolve_repository_ref(&ctx.client, &config.spec.repository, cfg_ns).await?;
    Ok((config, repo))
}

/// Best-effort, **positive-only** securityContext check for a backup source PVC. Lists the
/// workload pods mounting `claim` and, when the mover is *provably* compatible (root, or an
/// exact UID match with the workload), records `SecurityContextCompatible=True`. It NEVER
/// writes `False` or emits an Event: a securityContext-only heuristic can't see file modes, so
/// a UID mismatch is not proof of unreadability (world-readable data reads fine). The certain
/// `False`+Event comes only from [`assess_completed_backup`] (kopia's own output); the
/// advisory negative lives in the admission warning. Never returns an error.
async fn assess_backup_security_context(
    namespace: &str,
    backup: &Snapshot,
    claim: &str,
    sc: &k8s_openapi::api::core::v1::SecurityContext,
    psc: Option<&k8s_openapi::api::core::v1::PodSecurityContext>,
    ctx: &Context,
) {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::ListParams;

    let pods = match Api::<Pod>::namespaced(ctx.client.clone(), namespace)
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
    let mover = kopiur_api::secctx_compat::mover_identity(sc, psc);
    let identities = kopiur_api::secctx_compat::workload_identities(&pods, claim);
    if let kopiur_api::secctx_compat::MoverReadCompat::Compatible { .. } =
        kopiur_api::secctx_compat::assess_read_compat(&mover, &identities)
    {
        let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), namespace);
        set_security_context_compatible(
            &api,
            &backup.name_any(),
            backup,
            "the mover's UID can read the source (root, or an exact UID match with the workload)",
        )
        .await;
    }
    // Undecidable / likely-incompatible from securityContext alone → stay silent on the
    // reconcile path (no false alarms). The mover verifies it for real at runtime.
}

/// Upsert `SecurityContextCompatible=True` on a Snapshot (idempotent, no Event — a positive
/// confirmation, never an alarm).
async fn set_security_context_compatible(
    api: &Api<Snapshot>,
    name: &str,
    backup: &Snapshot,
    message: &str,
) {
    let existing = backup
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

/// `error_policy` for the `Snapshot` controller.
pub fn error_policy(backup: Arc<Snapshot>, err: &Error, ctx: Arc<Context>) -> Action {
    error_policy_for("Snapshot", backup.as_ref(), err, &ctx)
}
