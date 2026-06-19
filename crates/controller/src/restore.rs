//! The `Restore` reconciler (ADR §4.6, §4.7).
//!
//! Resolves the source (`snapshotRef` / `fromPolicy` / `identity`), pins
//! `status.resolved`, creates a restore mover `Job`, and handles the passive
//! populator mode (a PVC's `spec.dataSourceRef` points at the `Restore`).
//!
//! The source-mode dispatch is an **exhaustive `match`** over the externally
//! tagged `RestoreSource` enum (no `_ =>`), and [`default_on_missing`] /
//! [`populator_state`] are pure decisions, all unit-tested. The populator path
//! ([`drive_populator_restore`]) implements the full CSI volume-populator
//! handshake: restore into a controller-created **prime** PVC, then rebind its PV
//! to the claiming PVC (mirrors `kubernetes-csi/lib-volume-populator`).

use std::sync::Arc;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::api::ListParams;
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};

use kopiur_api::snapshot::Snapshot;
use kopiur_api::{
    OnMissingSnapshot, Restore, RestorePhase, RestoreSource, RestoreTarget, SnapshotPhase, validate,
};
use kopiur_mover::workspec::{
    MoverOptions, MoverWorkSpec, Operation, RepositoryConnect, ResolvedIdentity as MoverIdentity,
    RestoreOp, TargetRef,
};

use crate::config;
use crate::consts::{
    ALLOW_PRIVILEGED_MOVER_ACTION, API_VERSION, CREDENTIALS_AVAILABLE_CONDITION,
    CREDENTIALS_PROJECTED_REASON, MISSING_CREDENTIALS_REASON, MOVER_PERMITTED_CONDITION,
    PRIVILEGED_MOVER_NOT_PERMITTED_REASON, RESTORE_SECURITY_CONTEXT_COMPATIBLE_CONDITION,
    SECURITY_CONTEXT_COMPATIBLE_REASON,
};
use crate::context::Context;
use crate::error::{Error, Result, error_policy_for};
use crate::io::{self, ResolvedRepository};
use crate::jobs::{self, JobLimits, MoverJobInputs, VolumeMountSpec};

/// Which source mode a restore uses, as a stable string (mirrors
/// `RestoreSource::kind_str`, re-derived through an exhaustive match so a new
/// variant must be handled here too).
pub fn source_mode(source: &RestoreSource) -> &'static str {
    match source {
        RestoreSource::SnapshotRef(_) => "SnapshotRef",
        RestoreSource::FromPolicy(_) => "FromPolicy",
        RestoreSource::Identity(_) => "Identity",
    }
}

/// The default `onMissingSnapshot` for a source mode when the spec doesn't set
/// it (ADR §4.6 / SKILL "Restores fail closed"): `fromPolicy` defaults to
/// `Continue` (deploy-or-restore), everything else fails closed (`Fail`).
pub fn default_on_missing(source: &RestoreSource) -> OnMissingSnapshot {
    match source {
        RestoreSource::FromPolicy(_) => OnMissingSnapshot::Continue,
        RestoreSource::SnapshotRef(_) | RestoreSource::Identity(_) => OnMissingSnapshot::Fail,
    }
}

/// Effective `onMissingSnapshot`: explicit spec value wins, else the per-mode
/// default.
pub fn effective_on_missing(
    spec: Option<OnMissingSnapshot>,
    source: &RestoreSource,
) -> OnMissingSnapshot {
    spec.unwrap_or_else(|| default_on_missing(source))
}

/// State of the passive-populator handshake. Pure model of the §4.7 machine so
/// the reconcile loop can dispatch without re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopulatorState {
    /// `target: populator`: this `Restore` is a passive populator source, awaiting a
    /// PVC `dataSourceRef` to claim it (ADR-0005 §9).
    AwaitingClaim,
    /// An explicit `pvc`/`pvcRef` target: the operator drives the restore directly.
    DirectTarget,
}

/// Wall-clock duration (seconds) of a restore `Job` from its
/// `status.startTime`/`completionTime`. `None` if either is absent or the
/// interval is negative (clock skew). Pure. (`Time.0` is a jiff `Timestamp`.)
pub fn restore_job_duration_seconds(job: &k8s_openapi::api::batch::v1::Job) -> Option<i64> {
    let st = job.status.as_ref()?;
    let start = st.start_time.as_ref()?.0.as_second();
    let end = st.completion_time.as_ref()?.0.as_second();
    let secs = end - start;
    (secs >= 0).then_some(secs)
}

/// Decide the populator state from the restore `target` (ADR-0005 §9). Pure +
/// exhaustive over [`RestoreTarget`] (no `_ =>`), so a new target variant must be
/// considered here before it compiles: `populator` awaits a PVC `dataSourceRef`
/// claim; `pvc`/`pvcRef` is a direct, operator-driven restore.
pub fn populator_state(target: &RestoreTarget) -> PopulatorState {
    match target {
        RestoreTarget::Populator(_) => PopulatorState::AwaitingClaim,
        RestoreTarget::Pvc(_) | RestoreTarget::PvcRef(_) => PopulatorState::DirectTarget,
    }
}

/// Whether `phase` lets the reconcile-entry guard short-circuit. `Failed` always does.
/// `Completed` does for a DIRECT restore (the mover wrote the target PVC itself), but
/// NOT for a populator: there the mover stamps `Completed` on finishing the PRIME PVC
/// while the prime→consumer rebind is still pending, so it must fall through to
/// [`drive_populator_restore`]. Pure.
fn phase_is_terminal_at_guard(phase: RestorePhase, state: PopulatorState) -> bool {
    match phase {
        RestorePhase::Failed => true,
        RestorePhase::Completed => state == PopulatorState::DirectTarget,
        RestorePhase::Pending | RestorePhase::Resolving | RestorePhase::Restoring => false,
    }
}

/// Map a `Restore` phase to its kstatus [`io::ReadyOutcome`] (ADR-0005 §2), so
/// `kubectl wait --for=condition=Ready` and Flux/Argo health checks work on a
/// `Restore` exactly like every other kopiur CRD. Pure + exhaustive: a new phase
/// cannot compile until its Ready mapping is decided.
///
/// - `Completed` → `Ready` (the restore reached its desired state).
/// - `Failed` → `Stalled` (terminal: a Restore is one-shot; a NEW Restore is how
///   a retry happens).
/// - `Pending`/`Resolving`/`Restoring` → `Reconciling` (in flight).
pub fn restore_ready_outcome(phase: RestorePhase) -> io::ReadyOutcome {
    match phase {
        RestorePhase::Completed => io::ReadyOutcome::Ready,
        RestorePhase::Failed => io::ReadyOutcome::Stalled,
        RestorePhase::Pending | RestorePhase::Resolving | RestorePhase::Restoring => {
            io::ReadyOutcome::Reconciling
        }
    }
}

/// Build the `(phase, observedGeneration, conditions)` status JSON for a `Restore`
/// reaching `phase`, layering the kstatus Ready/Reconciling/Stalled conditions
/// (via [`restore_ready_outcome`] + [`io::set_ready`]) onto `base` — the caller's
/// condition set, normally the Restore's existing conditions plus any domain
/// condition (`Resolved`, `AwaitingClaim`, …) upserted for this transition. Every
/// status write goes through here so domain conditions survive phase writes (a
/// bare `conditions: [..]` array replace used to drop them) and every phase
/// transition carries Ready conditions (the job-success path used to write the
/// phase alone, so `kubectl wait --for=condition=Ready` and Flux healthChecks
/// could never gate on a completed Restore). Mirrors `snapshot_ready_status`.
fn restore_ready_status_on(
    restore: &Restore,
    base: &[Condition],
    phase: RestorePhase,
    reason: &str,
    message: &str,
) -> serde_json::Value {
    use kopiur_api::common::PhaseLabel;
    let generation = restore.metadata.generation;
    let conditions = io::set_ready(
        base,
        generation,
        restore_ready_outcome(phase),
        reason,
        message,
    );
    serde_json::json!({
        "phase": phase.label(),
        "observedGeneration": generation,
        "conditions": conditions,
    })
}

/// [`restore_ready_status_on`] over the Restore's existing conditions unchanged —
/// the common case where a transition has no domain condition of its own.
fn restore_ready_status(
    restore: &Restore,
    phase: RestorePhase,
    reason: &str,
    message: &str,
) -> serde_json::Value {
    restore_ready_status_on(
        restore,
        &existing_conditions(restore),
        phase,
        reason,
        message,
    )
}

/// True when the kstatus trio on `restore` already reflects `phase`'s outcome,
/// keyed on the one condition that is `True` for that outcome (`Ready` for
/// Completed, `Stalled` for Failed, `Reconciling` for in-flight). Checking the
/// distinctive condition suffices because [`io::set_ready`] always writes the
/// trio together. This is the terminal-gate heal's self-gate: checking the
/// PHASE alone is not enough, because the mover stamps the terminal phase
/// without conditions (so the conditions can still say `Reconciling` — or be
/// absent entirely — while the phase is already `Completed`).
fn kstatus_settled_for(restore: &Restore, phase: RestorePhase) -> bool {
    use crate::consts::{READY_CONDITION, RECONCILING_CONDITION, STALLED_CONDITION};
    let distinctive = match restore_ready_outcome(phase) {
        io::ReadyOutcome::Ready => READY_CONDITION,
        io::ReadyOutcome::Stalled => STALLED_CONDITION,
        io::ReadyOutcome::Reconciling => RECONCILING_CONDITION,
    };
    restore.status.as_ref().is_some_and(|s| {
        s.conditions
            .iter()
            .any(|c| c.type_ == distinctive && c.status == "True")
    })
}

/// The Restore's current status conditions (empty when no status yet).
fn existing_conditions(restore: &Restore) -> Vec<Condition> {
    restore
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default()
}

/// Reconcile a `Restore`.
#[tracing::instrument(skip(restore, ctx), fields(kind = "Restore", namespace = %restore.namespace().unwrap_or_default(), name = %restore.name_any()))]
pub async fn reconcile(restore: Arc<Restore>, ctx: Arc<Context>) -> Result<Action> {
    let start = std::time::Instant::now();
    let result = reconcile_inner(&restore, &ctx).await;
    ctx.metrics
        .record_reconcile("Restore", start.elapsed().as_secs_f64());
    record_restore_status_metrics(&restore, &ctx, result.is_ok()).await;
    result
}

/// Mirror a Restore's phase gauge. Zeroes it on deletion (so a Failed restore's
/// alert clears once the CR is gone) and re-reads the freshest status on success
/// — see the Snapshot equivalent for the rationale. (Restore *duration* is
/// recorded at the Job-completion site, not from status.)
async fn record_restore_status_metrics(restore: &Restore, ctx: &Context, ok: bool) {
    let (Some(ns), name) = (restore.namespace(), restore.name_any()) else {
        return;
    };
    if restore.metadata.deletion_timestamp.is_some() {
        ctx.metrics
            .clear_phase::<RestorePhase>("Restore", &ns, &name);
        return;
    }
    if !ok {
        return;
    }
    let api: Api<Restore> = Api::namespaced(ctx.client.clone(), &ns);
    if let Ok(Some(latest)) = api.get_opt(&name).await
        && let Some(phase) = latest.status.as_ref().and_then(|s| s.phase)
    {
        ctx.metrics.set_restore_phase(&ns, &name, phase);
    }
}

async fn reconcile_inner(restore: &Restore, ctx: &Context) -> Result<Action> {
    if let Err(e) = validate::validate_restore(&restore.spec) {
        return Err(Error::Validation(e.to_string()));
    }

    let namespace = restore
        .namespace()
        .ok_or_else(|| Error::Invariant("Restore has no namespace".into()))?;
    let name = restore.name_any();
    let api: Api<Restore> = Api::namespaced(ctx.client.clone(), &namespace);

    let state = populator_state(&restore.spec.target);

    // Already terminal: a Restore is one-shot. Once Completed/Failed there is
    // nothing left to do until the spec changes, so don't re-resolve, re-pin a
    // fresh timestamp, or re-write the phase — each of which would churn status and
    // self-trigger another reconcile (the same hot-loop class as the repo bug).
    // Mirrors the Snapshot reconciler's terminal discipline. (A `Completed` populator
    // is NOT terminal here — see `phase_is_terminal_at_guard`.)
    match restore.status.as_ref().and_then(|s| s.phase) {
        Some(phase) if phase_is_terminal_at_guard(phase, state) => {
            // The kstatus conditions come from the controller's transition patch
            // in `drive_direct_restore` — which the MOVER's own terminal `phase`
            // stamp races past in the common case (its in-cluster PATCH carries
            // `phase: Completed`/`Failed` + logTail/failure but no conditions;
            // the Job-completion reconcile then already sees the terminal phase
            // and lands HERE, never in the Job branch). Heal once: patch ONLY
            // phase + observedGeneration + conditions (`restore_ready_status`
            // carries nothing else, so the merge preserves the mover-written
            // logTail/failure/progress and the pinned resolution). Self-gated by
            // `kstatus_settled_for`, so a healed Restore never re-patches.
            if !kstatus_settled_for(restore, phase) {
                let (reason, message) = if phase == RestorePhase::Completed {
                    (
                        "RestoreSucceeded",
                        "the restore mover completed; the snapshot data was written \
                         into the target",
                    )
                } else {
                    (
                        "MoverJobFailed",
                        "the restore mover reported a terminal failure; see \
                         status.failure / status.logTail for the cause, fix it, and \
                         create a NEW Restore — a Failed Restore is terminal and \
                         never retries",
                    )
                };
                io::patch_status(
                    &api,
                    &name,
                    restore_ready_status(restore, phase, reason, message),
                )
                .await?;
            }
            return Ok(Action::requeue(std::time::Duration::from_secs(600)));
        }
        _ => {}
    }

    // §3: pin the resolved source kind to status so the SOURCE printer column shows
    // where the restore reads from. Deterministic (from the spec source variant), so
    // an unchanged value is a no-op patch.
    let source_kind = source_mode(&restore.spec.source);
    if restore
        .status
        .as_ref()
        .and_then(|s| s.source_kind.as_deref())
        != Some(source_kind)
    {
        io::patch_status(
            &api,
            &name,
            serde_json::json!({ "sourceKind": source_kind }),
        )
        .await?;
    }

    let on_missing = effective_on_missing(
        restore
            .spec
            .policy
            .as_ref()
            .and_then(|p| p.on_missing_snapshot),
        &restore.spec.source,
    );

    // ADR §4.6: the resolution is pinned ONCE and never re-resolved — a restore
    // must not silently retarget when newer snapshots appear mid-flight. Reuse a
    // previously pinned id; resolve only while no pin exists yet.
    let pinned = restore.status.as_ref().and_then(|s| s.resolved.as_ref());
    let resolved_source = if let Some(id) = pinned.and_then(|r| r.kopia_snapshot_id.clone()) {
        ResolvedSource {
            kopia_snapshot_id: id,
            snapshot_ref: pinned.and_then(|r| r.snapshot_ref.clone()),
            identity: pinned.and_then(|r| r.identity.clone()),
        }
    } else {
        match resolve_snapshot(ctx, restore, &namespace).await? {
            Some(res) => {
                // Pin the FULL resolution (id + provenance + timestamp) exactly
                // once; the no-pin check above makes this a single write, so it
                // cannot churn status.
                let mut resolved = serde_json::json!({
                    "kopiaSnapshotID": res.kopia_snapshot_id,
                    "pinnedAt": chrono::Utc::now().to_rfc3339(),
                });
                if let Some(r) = &res.snapshot_ref {
                    resolved["snapshotRef"] = serde_json::to_value(r)?;
                }
                if let Some(i) = &res.identity {
                    resolved["identity"] = serde_json::to_value(i)?;
                }
                let mut status = restore_ready_status(
                    restore,
                    RestorePhase::Resolving,
                    "SourceResolved",
                    "the restore source resolved to a concrete kopia snapshot \
                     (pinned to status.resolved)",
                );
                status["resolved"] = resolved;
                io::patch_status(&api, &name, status).await?;
                res
            }
            None => {
                // No snapshot matched. While the `waitTimeout` window (anchored at
                // the Restore's creation) is open, keep waiting instead of giving
                // up — `onMissingSnapshot` applies only once the window closes
                // (ADR §4.6 G7).
                let now = chrono::Utc::now().timestamp();
                let created = restore
                    .metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|t| t.0.as_second())
                    .unwrap_or(now);
                let wait_timeout = restore
                    .spec
                    .policy
                    .as_ref()
                    .and_then(|p| p.wait_timeout.as_deref());
                if let Some(remaining) = wait_remaining_secs(created, wait_timeout, now) {
                    // Static message (no countdown): an identical re-patch is a
                    // server-side no-op, so polling here cannot churn status.
                    let msg = format!(
                        "no snapshot matched the restore source yet; waiting up to \
                         waitTimeout ({}) from creation for it to appear before \
                         applying onMissingSnapshot",
                        wait_timeout.unwrap_or_default()
                    );
                    let conditions = io::upsert_condition(
                        &existing_conditions(restore),
                        "Resolved",
                        false,
                        "WaitingForSnapshot",
                        &msg,
                        restore.metadata.generation,
                    );
                    io::patch_status(
                        &api,
                        &name,
                        restore_ready_status_on(
                            restore,
                            &conditions,
                            RestorePhase::Pending,
                            "WaitingForSnapshot",
                            &msg,
                        ),
                    )
                    .await?;
                    return Ok(Action::requeue(std::time::Duration::from_secs(
                        remaining.clamp(1, 15),
                    )));
                }
                // Window closed (or none configured): honor the closed enum exhaustively.
                return match on_missing {
                    OnMissingSnapshot::Fail => {
                        let msg = "no snapshot matched the restore source within the \
                                   waitTimeout window; fix spec.source (or create the missing \
                                   snapshot) and create a NEW Restore — a Failed Restore is \
                                   terminal and never retries";
                        let conditions = io::upsert_condition(
                            &existing_conditions(restore),
                            "Resolved",
                            false,
                            "SnapshotNotFound",
                            msg,
                            restore.metadata.generation,
                        );
                        io::patch_status(
                            &api,
                            &name,
                            restore_ready_status_on(
                                restore,
                                &conditions,
                                RestorePhase::Failed,
                                "SnapshotNotFound",
                                msg,
                            ),
                        )
                        .await?;
                        Err(Error::MissingDependency(
                            "no snapshot matched restore source".into(),
                        ))
                    }
                    OnMissingSnapshot::Continue => {
                        // Deploy-or-restore: nothing to restore, complete cleanly.
                        let msg = "no snapshot found; continuing without restoring \
                                   (deploy-or-restore)";
                        let conditions = io::upsert_condition(
                            &existing_conditions(restore),
                            "Resolved",
                            true,
                            "NoSnapshotContinue",
                            msg,
                            restore.metadata.generation,
                        );
                        io::patch_status(
                            &api,
                            &name,
                            restore_ready_status_on(
                                restore,
                                &conditions,
                                RestorePhase::Completed,
                                "NoSnapshotContinue",
                                msg,
                            ),
                        )
                        .await?;
                        Ok(Action::requeue(std::time::Duration::from_secs(600)))
                    }
                };
            }
        }
    };

    match state {
        PopulatorState::DirectTarget => {
            drive_direct_restore(ctx, restore, &api, &namespace, &name, &resolved_source).await
        }
        PopulatorState::AwaitingClaim => {
            drive_populator_restore(ctx, restore, &api, &namespace, &name, &resolved_source).await
        }
    }
}

/// Park a populator `Restore` in `AwaitingClaim=True` / `Pending` with `reason`+`msg`
/// (no claiming PVC yet, or a WaitForFirstConsumer claim that hasn't been scheduled).
/// Mirrors the pre-handshake stub's status shape so consumers see a stable surface.
async fn park_awaiting_claim(
    api: &Api<Restore>,
    restore: &Restore,
    name: &str,
    reason: &str,
    msg: &str,
) -> Result<()> {
    let conditions = io::upsert_condition(
        &existing_conditions(restore),
        "AwaitingClaim",
        true,
        reason,
        msg,
        restore.metadata.generation,
    );
    let mut status =
        restore_ready_status_on(restore, &conditions, RestorePhase::Pending, reason, msg);
    status["target"] = serde_json::json!({ "pvcPrime": "awaiting-claim" });
    io::patch_status(api, name, status).await
}

/// CSI volume-populator handshake for `target.populator: {}` (ADR-0005 §9): restore
/// the snapshot into a prime PVC, then rebind its PV to the claiming PVC (mirrors
/// kubernetes-csi/lib-volume-populator). Each step keys off observed state so requeues
/// are idempotent; the prime PV is set `Retain` before its PVC is deleted so the
/// volume survives the swap.
async fn drive_populator_restore(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    source: &ResolvedSource,
) -> Result<Action> {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;

    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);

    // The claiming PVC: one in this namespace whose dataSourceRef targets this
    // Restore. (dataSourceRef is namespace-local; a cross-namespace claim would need
    // a ReferenceGrant, out of scope here.)
    let consumer = pvc_api
        .list(&kube::api::ListParams::default())
        .await?
        .items
        .into_iter()
        .find(|pvc| pvc_claims_restore(pvc, name));

    let Some(consumer) = consumer else {
        park_awaiting_claim(
            api,
            restore,
            name,
            "AwaitingPvcDataSourceRef",
            "passive populator: awaiting a PVC dataSourceRef to claim this Restore",
        )
        .await?;
        return Ok(Action::requeue(std::time::Duration::from_secs(30)));
    };

    let consumer_name = consumer.name_any();
    let consumer_uid = consumer
        .uid()
        .ok_or_else(|| Error::Invariant("claiming PVC has no uid".into()))?;
    let prime_name = format!("prime-{consumer_uid}");
    let populate_job = format!("{name}-populate");

    let phase = restore.status.as_ref().and_then(|s| s.phase);
    // Terminal only once the consumer is bound: the mover stamps `Completed` on finishing
    // the prime PVC, but the rebind may still be pending. Once bound, `finalize_populator`
    // has stripped the rebind annotation so the `our_rebound_pv` probe below would no
    // longer recognize our PV — short-circuit to avoid recreating the prime. While
    // `Completed` but not yet bound, fall through to issue/await the rebind.
    if phase == Some(RestorePhase::Completed) && pvc_is_bound(&consumer) {
        return Ok(Action::requeue(std::time::Duration::from_secs(600)));
    }

    // The PV our populator earmarked for this consumer: claimRef → the consumer AND our
    // rebind annotation. `Some` proves the prime→consumer rebind was already issued — so
    // we never recreate the prime past that point, and we complete ONLY once the consumer
    // is bound to THAT PV (not to an empty one a non-populator-aware provisioner handed it).
    if let Some(pv_name) = our_rebound_pv(ctx, namespace, &consumer_name).await? {
        if pvc_is_bound(&consumer)
            && consumer
                .spec
                .as_ref()
                .and_then(|s| s.volume_name.as_deref())
                == Some(pv_name.as_str())
        {
            finalize_populator(ctx, namespace, &populate_job, &prime_name, Some(&pv_name)).await?;
            io::patch_status(
                api,
                name,
                restore_ready_status(
                    restore,
                    RestorePhase::Completed,
                    crate::consts::RESTORE_POPULATED_REASON,
                    "populator: restored the snapshot into the claiming PVC and rebound the volume",
                ),
            )
            .await?;
            return Ok(Action::requeue(std::time::Duration::from_secs(600)));
        }
        // Rebind issued; wait for the PV controller to bind our PV to the consumer.
        return Ok(Action::requeue(std::time::Duration::from_secs(5)));
    }

    // WaitForFirstConsumer gate: a late-binding StorageClass only provisions once a
    // pod schedules the claim (the `selected-node` annotation appears). Provision the
    // prime PVC on the SAME node so its PV lands where the workload will run.
    let selected_node = consumer
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("volume.kubernetes.io/selected-node").cloned());
    if consumer_storage_class_is_wffc(ctx, &consumer).await? && selected_node.is_none() {
        park_awaiting_claim(
            api,
            restore,
            name,
            "AwaitingPodSchedule",
            "populator: waiting for a pod to schedule the claiming PVC (WaitForFirstConsumer)",
        )
        .await?;
        return Ok(Action::requeue(std::time::Duration::from_secs(15)));
    }

    // Provision the prime PVC (mirrors the claim's spec, no dataSourceRef), then run
    // the restore mover into it.
    ensure_prime_pvc(
        ctx,
        restore,
        namespace,
        &prime_name,
        &consumer,
        selected_node.as_deref(),
    )
    .await?;

    match run_restore_mover(
        ctx,
        restore,
        api,
        namespace,
        &populate_job,
        &prime_name,
        source,
    )
    .await?
    {
        MoverOutcome::Running { created } => {
            let phase = restore.status.as_ref().and_then(|s| s.phase);
            if created || phase != Some(RestorePhase::Restoring) {
                io::patch_status(
                    api,
                    name,
                    restore_ready_status(
                        restore,
                        RestorePhase::Restoring,
                        "PopulatingPrimePvc",
                        "populator: restoring the snapshot into the prime PVC",
                    ),
                )
                .await?;
            }
            return Ok(Action::requeue(std::time::Duration::from_secs(15)));
        }
        MoverOutcome::Failed => {
            let phase = restore.status.as_ref().and_then(|s| s.phase);
            if phase != Some(RestorePhase::Failed) {
                io::patch_status(
                    api,
                    name,
                    restore_ready_status(
                        restore,
                        RestorePhase::Failed,
                        "MoverJobFailed",
                        "the populator restore mover Job failed; see the Job/pod logs, fix the \
                         cause, and re-create the claiming PVC — a Failed Restore is terminal",
                    ),
                )
                .await?;
            }
            return Ok(Action::requeue(std::time::Duration::from_secs(120)));
        }
        MoverOutcome::Wedged { message } => {
            let phase = restore.status.as_ref().and_then(|s| s.phase);
            if phase != Some(RestorePhase::Failed) {
                io::patch_status(
                    api,
                    name,
                    restore_ready_status(restore, RestorePhase::Failed, "MoverPodWedged", &message),
                )
                .await?;
            }
            return Ok(Action::requeue(std::time::Duration::from_secs(120)));
        }
        MoverOutcome::Succeeded { .. } => {}
    }

    // Mover done: hand the prime PV to the consumer, then requeue so the next pass
    // observes the bind and finalizes. `rebind_prime_to_consumer` returns `false`
    // (requeue soon) while the prime PV hasn't appeared yet.
    if !rebind_prime_to_consumer(ctx, namespace, &prime_name, &consumer_name, &consumer_uid).await?
    {
        return Ok(Action::requeue(std::time::Duration::from_secs(5)));
    }
    Ok(Action::requeue(std::time::Duration::from_secs(5)))
}

/// The selected-node annotation a late-binding (`WaitForFirstConsumer`) PVC carries
/// once the scheduler picks a node for its first consuming pod.
const SELECTED_NODE_ANNOTATION: &str = "volume.kubernetes.io/selected-node";
/// Annotation kopiur stamps on a prime PV while it is temporarily forced to `Retain`
/// during the rebind — carries the PV's ORIGINAL reclaim policy so
/// [`finalize_populator`] can restore it once the consumer binds.
const PRIME_ORIGINAL_RECLAIM_ANNOTATION: &str =
    "kopiur.home-operations.com/populator-original-reclaim-policy";

/// The `PersistentVolume` our populator earmarked for `consumer_name`: its `claimRef`
/// targets the consumer and it carries [`PRIME_ORIGINAL_RECLAIM_ANNOTATION`] (stamped
/// during the rebind). `Some` ⇒ the prime→consumer rebind has been issued.
async fn our_rebound_pv(
    ctx: &Context,
    namespace: &str,
    consumer_name: &str,
) -> Result<Option<String>> {
    use k8s_openapi::api::core::v1::PersistentVolume;
    let pv_api: Api<PersistentVolume> = Api::all(ctx.client.clone());
    Ok(pv_api
        .list(&kube::api::ListParams::default())
        .await?
        .items
        .into_iter()
        .find(|pv| {
            pv.metadata
                .annotations
                .as_ref()
                .is_some_and(|a| a.contains_key(PRIME_ORIGINAL_RECLAIM_ANNOTATION))
                && pv
                    .spec
                    .as_ref()
                    .and_then(|s| s.claim_ref.as_ref())
                    .is_some_and(|cr| {
                        cr.name.as_deref() == Some(consumer_name)
                            && cr.namespace.as_deref() == Some(namespace)
                    })
        })
        .map(|pv| pv.name_any()))
}

/// True when `pvc.spec.dataSourceRef` claims the populator `Restore` named
/// `restore_name` (apiGroup `kopiur.home-operations.com`, kind `Restore`). Pure.
fn pvc_claims_restore(
    pvc: &k8s_openapi::api::core::v1::PersistentVolumeClaim,
    restore_name: &str,
) -> bool {
    pvc.spec
        .as_ref()
        .and_then(|s| s.data_source_ref.as_ref())
        .is_some_and(|dsr| {
            dsr.kind == "Restore"
                && dsr.name == restore_name
                && dsr.api_group.as_deref() == Some("kopiur.home-operations.com")
        })
}

/// True once `pvc` is bound to a `PersistentVolume`. Pure.
fn pvc_is_bound(pvc: &k8s_openapi::api::core::v1::PersistentVolumeClaim) -> bool {
    pvc.spec
        .as_ref()
        .and_then(|s| s.volume_name.as_deref())
        .is_some_and(|v| !v.is_empty())
        || pvc.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Bound")
}

/// Whether the claim's `StorageClass` binds late (`WaitForFirstConsumer`), so the
/// prime PVC must wait for the scheduler to pick a node. A claim with no class named
/// is treated as `Immediate`.
async fn consumer_storage_class_is_wffc(
    ctx: &Context,
    consumer: &k8s_openapi::api::core::v1::PersistentVolumeClaim,
) -> Result<bool> {
    use k8s_openapi::api::storage::v1::StorageClass;
    let Some(scn) = consumer
        .spec
        .as_ref()
        .and_then(|s| s.storage_class_name.clone())
    else {
        return Ok(false);
    };
    let sc_api: Api<StorageClass> = Api::all(ctx.client.clone());
    Ok(sc_api
        .get_opt(&scn)
        .await?
        .and_then(|sc| sc.volume_binding_mode)
        .as_deref()
        == Some("WaitForFirstConsumer"))
}

/// Create the prime PVC if absent: the claim's spec with the data source stripped (so
/// a provisioner gives it a fresh PV), pinned to `selected_node` for late binding, and
/// owned by the Restore. Idempotent.
async fn ensure_prime_pvc(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
    prime_name: &str,
    consumer: &k8s_openapi::api::core::v1::PersistentVolumeClaim,
    selected_node: Option<&str>,
) -> Result<()> {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
    if pvc_api.get_opt(prime_name).await?.is_some() {
        return Ok(());
    }
    let mut spec = consumer
        .spec
        .clone()
        .ok_or_else(|| Error::Invariant("claiming PVC has no spec".into()))?;
    // The prime PVC must be provisioned normally, NOT via the populator — strip the
    // data source and any bound-volume hints.
    spec.data_source = None;
    spec.data_source_ref = None;
    spec.volume_name = None;
    spec.selector = None;
    let mut annotations = std::collections::BTreeMap::new();
    if let Some(node) = selected_node {
        annotations.insert(SELECTED_NODE_ANNOTATION.to_string(), node.to_string());
    }
    let prime = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(prime_name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(io::child_labels(&[(
                crate::consts::OP_LABEL,
                crate::consts::OP_RESTORE_POPULATE,
            )])),
            annotations: (!annotations.is_empty()).then_some(annotations),
            owner_references: Some(vec![io::owner_ref_for(restore, "Restore")?]),
            ..Default::default()
        },
        spec: Some(spec),
        status: None,
    };
    match pvc_api
        .create(&kube::api::PostParams::default(), &prime)
        .await
    {
        Ok(_) => {
            tracing::info!(prime = %prime_name, %namespace, "created populator prime PVC");
            Ok(())
        }
        // Lost a create race with another reconcile — the PVC exists, which is all
        // this function guarantees.
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Hand the prime PV to the claiming PVC: set it `Retain` (stashing the original
/// policy in an annotation), repoint `claimRef` at the consumer (clearing
/// `resourceVersion`), then delete the prime PVC. Returns `false` while the prime PV
/// isn't provisioned yet (caller requeues), `true` once the rebind is issued.
async fn rebind_prime_to_consumer(
    ctx: &Context,
    namespace: &str,
    prime_name: &str,
    consumer_name: &str,
    consumer_uid: &str,
) -> Result<bool> {
    use k8s_openapi::api::core::v1::{PersistentVolume, PersistentVolumeClaim};
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
    let pv_api: Api<PersistentVolume> = Api::all(ctx.client.clone());

    // Prime gone → rebind completed on a prior pass.
    let Some(prime) = pvc_api.get_opt(prime_name).await? else {
        return Ok(true);
    };
    // PV not provisioned/bound to the prime yet → requeue soon.
    let Some(pv_name) = prime
        .spec
        .as_ref()
        .and_then(|s| s.volume_name.clone())
        .filter(|v| !v.is_empty())
    else {
        return Ok(false);
    };
    let Some(pv) = pv_api.get_opt(&pv_name).await? else {
        return Ok(false);
    };

    let already = pv
        .spec
        .as_ref()
        .and_then(|s| s.claim_ref.as_ref())
        .is_some_and(|cr| {
            cr.name.as_deref() == Some(consumer_name) && cr.uid.as_deref() == Some(consumer_uid)
        });
    if !already {
        let original = pv
            .spec
            .as_ref()
            .and_then(|s| s.persistent_volume_reclaim_policy.clone())
            .unwrap_or_else(|| "Delete".to_string());
        let patch = serde_json::json!({
            "metadata": { "annotations": { PRIME_ORIGINAL_RECLAIM_ANNOTATION: original } },
            "spec": {
                "persistentVolumeReclaimPolicy": "Retain",
                "claimRef": {
                    "apiVersion": "v1",
                    "kind": "PersistentVolumeClaim",
                    "namespace": namespace,
                    "name": consumer_name,
                    "uid": consumer_uid,
                    // RFC 7386 merge: null removes the stale resourceVersion so the PV
                    // controller doesn't reject the rebind on a version mismatch.
                    "resourceVersion": null,
                },
            },
        });
        pv_api
            .patch(
                &pv_name,
                &kube::api::PatchParams::default(),
                &kube::api::Patch::Merge(patch),
            )
            .await?;
        tracing::info!(pv = %pv_name, consumer = %consumer_name, "populator: rebound prime PV to the claiming PVC");
    }
    // Safe now: the PV is Retain + claimRef→consumer, so deleting the prime PVC frees
    // the name without reaping the volume; the PV controller binds it to the consumer.
    let _ = pvc_api
        .delete(prime_name, &kube::api::DeleteParams::default())
        .await;
    Ok(true)
}

/// Finalize: restore the bound PV's original reclaim policy (stashed during rebind),
/// then GC the populate Job/ConfigMap and any leftover prime PVC.
async fn finalize_populator(
    ctx: &Context,
    namespace: &str,
    populate_job: &str,
    prime_name: &str,
    bound_pv: Option<&str>,
) -> Result<()> {
    use k8s_openapi::api::batch::v1::Job;
    use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolume, PersistentVolumeClaim};
    if let Some(pv_name) = bound_pv {
        let pv_api: Api<PersistentVolume> = Api::all(ctx.client.clone());
        if let Some(pv) = pv_api.get_opt(pv_name).await?
            && let Some(orig) = pv
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(PRIME_ORIGINAL_RECLAIM_ANNOTATION))
                .cloned()
        {
            let patch = serde_json::json!({
                "metadata": { "annotations": { PRIME_ORIGINAL_RECLAIM_ANNOTATION: serde_json::Value::Null } },
                "spec": { "persistentVolumeReclaimPolicy": orig },
            });
            pv_api
                .patch(
                    pv_name,
                    &kube::api::PatchParams::default(),
                    &kube::api::Patch::Merge(patch),
                )
                .await?;
        }
    }
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    let cm_api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), namespace);
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
    let bg = kube::api::DeleteParams {
        propagation_policy: Some(kube::api::PropagationPolicy::Background),
        ..Default::default()
    };
    let _ = job_api.delete(populate_job, &bg).await;
    let _ = cm_api
        .delete(populate_job, &kube::api::DeleteParams::default())
        .await;
    let _ = pvc_api
        .delete(prime_name, &kube::api::DeleteParams::default())
        .await;
    Ok(())
}

/// Drive a restore-with-explicit-target: create the restore mover Job (writing
/// into the target PVC), then track it to terminal.
async fn drive_direct_restore(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    source: &ResolvedSource,
) -> Result<Action> {
    // Resolve the target PVC for the restore Job. DirectTarget is only reached for
    // an explicit PVC target (populator routes to AwaitingClaim in the reconcile
    // dispatch). Exhaustive over RestoreTarget so a new variant must be considered.
    let target_pvc = match &restore.spec.target {
        RestoreTarget::PvcRef(r) => r.name.clone(),
        // `target.pvc` means the operator CREATES the PVC (ADR §3.6) — without
        // this the mover Job references a claim nobody made and sits Pending
        // forever (FailedScheduling: persistentvolumeclaim not found).
        RestoreTarget::Pvc(t) => {
            ensure_restore_target_pvc(ctx, namespace, t).await?;
            t.name.clone()
        }
        RestoreTarget::Populator(_) => {
            return Err(Error::Invariant(
                "DirectTarget restore reached with a populator target (should route to \
                 AwaitingClaim)"
                    .into(),
            ));
        }
    };

    // The Job is named after the Restore and writes into the explicit target PVC;
    // the helper creates/tracks it, the phase writes stay here.
    let phase = restore.status.as_ref().and_then(|s| s.phase);
    match run_restore_mover(ctx, restore, api, namespace, name, &target_pvc, source).await? {
        MoverOutcome::Succeeded { duration_secs } => {
            if let Some(secs) = duration_secs {
                ctx.metrics.set_restore_duration(namespace, name, secs);
            }
            if phase != Some(RestorePhase::Completed) {
                io::patch_status(
                    api,
                    name,
                    restore_ready_status(
                        restore,
                        RestorePhase::Completed,
                        "RestoreSucceeded",
                        "the restore mover Job completed; the snapshot data was \
                         written into the target",
                    ),
                )
                .await?;
            }
            Ok(Action::requeue(std::time::Duration::from_secs(600)))
        }
        MoverOutcome::Failed => {
            if phase != Some(RestorePhase::Failed) {
                io::patch_status(
                    api,
                    name,
                    restore_ready_status(
                        restore,
                        RestorePhase::Failed,
                        "MoverJobFailed",
                        "the restore mover Job failed; see the Job/pod logs for the \
                         cause, fix it, and create a NEW Restore — a Failed Restore \
                         is terminal and never retries",
                    ),
                )
                .await?;
            }
            Ok(Action::requeue(std::time::Duration::from_secs(120)))
        }
        MoverOutcome::Running { created } => {
            let (target_phase, reason, msg) = if created {
                (
                    RestorePhase::Restoring,
                    "MoverJobCreated",
                    "created the restore mover Job",
                )
            } else {
                (
                    RestorePhase::Restoring,
                    "MoverJobRunning",
                    "the restore mover Job is in flight",
                )
            };
            // A new Job always writes; a poll only on a phase flip.
            if created || phase != Some(RestorePhase::Restoring) {
                io::patch_status(
                    api,
                    name,
                    restore_ready_status(restore, target_phase, reason, msg),
                )
                .await?;
            }
            Ok(Action::requeue(std::time::Duration::from_secs(30)))
        }
        MoverOutcome::Wedged { message } => {
            if phase != Some(RestorePhase::Failed) {
                io::patch_status(
                    api,
                    name,
                    restore_ready_status(restore, RestorePhase::Failed, "MoverPodWedged", &message),
                )
                .await?;
            }
            Ok(Action::requeue(std::time::Duration::from_secs(120)))
        }
    }
}

/// Observed state of a restore mover Job, returned by [`run_restore_mover`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum MoverOutcome {
    /// Not yet terminal. `created` is `true` only on the reconcile that applied it.
    Running { created: bool },
    /// Completed; `duration_secs` from its start/completion times.
    Succeeded { duration_secs: Option<i64> },
    /// Terminal failure (the mover Job reported failure).
    Failed,
    /// The mover pod can't START past the pod-startup deadline (impossible
    /// securityContext, bad image, unschedulable); the helper reaped the Job. `message`
    /// is the ready-to-surface explanation.
    Wedged { message: String },
}

/// Build + apply the restore mover Job named `job_name` (writing `snapshot_id` into
/// `target_pvc`, mounted read-write at `/restore`) and report its [`MoverOutcome`].
/// Idempotent: an existing Job is tracked to terminal, never re-applied. The caller
/// owns the status/phase writes.
async fn run_restore_mover(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    job_name: &str,
    target_pvc: &str,
    source: &ResolvedSource,
) -> Result<MoverOutcome> {
    use k8s_openapi::api::batch::v1::Job;
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    if let Some(job) = job_api.get_opt(job_name).await? {
        return Ok(match crate::snapshot::job_terminal_state(&job) {
            Some(true) => MoverOutcome::Succeeded {
                duration_secs: restore_job_duration_seconds(&job),
            },
            Some(false) => MoverOutcome::Failed,
            // A mover that can't START (impossible securityContext, bad image,
            // unschedulable) never terminates, so backoffLimit never trips — fail fast
            // past the pod-startup deadline instead of hanging to the 48h backstop.
            None => {
                let grace = kopiur_api::common::pod_startup_deadline_seconds(
                    restore.spec.failure_policy.as_ref(),
                );
                if let io::WedgedVerdict::Wedged { reason, message } =
                    io::wedged_pod_verdict(&ctx.client, namespace, job_name, grace).await?
                {
                    // Reap the wedged Job (cascade) so the kubelet stops retrying.
                    let _ = job_api
                        .delete(job_name, &kube::api::DeleteParams::background())
                        .await;
                    MoverOutcome::Wedged {
                        message: crate::snapshot::wedged_pod_message(&reason, &message, grace),
                    }
                } else {
                    MoverOutcome::Running { created: false }
                }
            }
        });
    }

    let target_path = "/restore".to_string();
    // Status patches and the work-spec `target_ref` reference the Restore itself; the
    // Job/ConfigMap/cache are named after `job_name` (`<restore>-populate` for the
    // populator path) so the two paths never collide.
    let restore_name = restore.name_any();
    let name = restore_name.as_str();

    // Resolve the repository for the restore Job.
    let repo = resolve_restore_repository(ctx, restore, namespace).await?;

    // The restore mover Job runs in this (workload) namespace: resolve its run
    // identity here — the user's workload-identity SA (preflighted + bound to the
    // mover role) or the minted mover SA — then resolve the credential Secret(s)
    // it loads via envFrom — verifying the user-managed ones are present, or (with
    // `spec.credentialProjection`) projecting the repository's Secret(s) here owned
    // by this Restore. A problem surfaces as a clear condition + Event (ADR §4.12).
    let mover_identity = match io::ensure_mover_identity(
        &ctx.client,
        namespace,
        &[&repo.backend],
        ctx.mover_service_account.as_deref(),
        &ctx.mover_role_kind,
        &ctx.mover_clusterrole,
    )
    .await
    {
        Ok(identity) => identity,
        Err(Error::MissingDependency(msg)) => {
            let existing = restore
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
                restore.metadata.generation,
            );
            io::patch_status(
                api,
                name,
                serde_json::json!({ "phase": "Pending", "conditions": conditions }),
            )
            .await?;
            io::publish_missing_sa_event(ctx, restore, &msg).await;
            return Err(Error::MissingDependency(msg));
        }
        Err(e) => return Err(e),
    };

    // Resolve the restore mover's EFFECTIVE security context once (explicit, or
    // inherited from a workload pod via `inheritSecurityContextFrom`). Both the gate
    // and the Job use it, so an inherited root context is gated like an explicit one.
    // The effective container + pod security contexts — explicit, or both inherited
    // from a workload pod via `inheritSecurityContextFrom`.
    // Restore has no backup *source* PVC; `pvcConsumer` is backup-only (validator-rejected
    // for restore), so pass None.
    let (effective_sc, effective_pod_sc) = io::resolve_mover_security_contexts(
        &ctx.client,
        namespace,
        restore.spec.mover.as_ref(),
        None,
    )
    .await?;
    let privileged_mode = restore.spec.mover.as_ref().and_then(|m| m.privileged_mode);

    // Field-wise merge the repository's moverDefaults under the recipe's effective
    // contexts/resources/cache (`hardened ⊂ moverDefaults ⊂ recipe`, ADR-0004 §1/§2).
    // The gate and the Job both run on the MERGED result.
    let resolved_mover = kopiur_api::common::resolve_mover(
        repo.mover_defaults.as_ref(),
        effective_sc.as_ref(),
        effective_pod_sc.as_ref(),
        restore
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.resources.as_ref()),
        restore.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
        restore
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.ttl_seconds_after_finished),
    );

    // Privileged-mover gate (ADR §4.11/§G16, VolSync-parity): an elevated restore mover
    // (root/privileged/added caps/`privilegedMode`, container- OR pod-level) requires the
    // target namespace to opt in via the `kopiur.home-operations.com/privileged-movers`
    // annotation — a tenant there could otherwise reuse the minted mover SA at that
    // privilege. Refuse with a clear `MoverPermitted=False` condition + Event otherwise.
    // Mirrors the Snapshot gate.
    if kopiur_api::common::requires_privilege_resolved(
        Some(&resolved_mover.security_context),
        resolved_mover.pod_security_context.as_ref(),
        privileged_mode,
    ) && !io::namespace_allows_privileged_movers(&ctx.client, namespace).await?
    {
        let sa = ctx
            .mover_service_account
            .as_deref()
            .unwrap_or(config::DEFAULT_MOVER_NAME);
        let msg = io::privileged_mover_message("Restore", name, namespace, sa);
        let existing = restore
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
            restore.metadata.generation,
        );
        io::patch_status(
            api,
            name,
            serde_json::json!({ "phase": "Pending", "conditions": conditions }),
        )
        .await?;
        io::publish_warning_event(
            ctx,
            restore,
            PRIVILEGED_MOVER_NOT_PERMITTED_REASON,
            ALLOW_PRIVILEGED_MOVER_ACTION,
            &msg,
        )
        .await;
        // The fix is an out-of-band namespace annotation; the Namespace watch
        // (`watch::namespace_to_restores`) re-enqueues this Restore the moment
        // the opt-in lands, so the requeue is only a watch-desync backstop.
        return Err(Error::BlockedOnGrant(msg));
    }
    // Permitted: clear any stale `MoverPermitted=False` from a prior reconcile.
    if let Some(conds) = restore.status.as_ref().map(|s| s.conditions.as_slice())
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
            restore.metadata.generation,
        );
        io::patch_status(api, name, serde_json::json!({ "conditions": conditions })).await?;
    }

    // Restore-direction securityContext (positive-only): confirm `True` when the future
    // consumer of the target PVC can read what the mover writes (matching UID / shared fsGroup
    // on the fresh volume). Never writes `False` — restore has no certain signal, so the
    // advisory negative lives in the admission warning. Never fatal.
    assess_restore_security_context(
        namespace,
        restore,
        target_pvc,
        &resolved_mover.security_context,
        resolved_mover.pod_security_context.as_ref(),
        ctx,
    )
    .await;

    let owner = io::owner_ref_for(restore, "Restore")?;
    let repo_ref = restore.spec.repository.as_ref();
    let creds = match io::resolve_mover_creds_for(
        &ctx.client,
        namespace,
        name,
        &owner,
        &repo,
        restore
            .spec
            .credential_projection
            .as_ref()
            .is_some_and(|p| p.enabled),
        repo_ref
            .map(|r| io::repo_kind_str(r.kind))
            .unwrap_or("Repository"),
        repo_ref
            .map(|r| r.name.as_str())
            .unwrap_or("(from source config)"),
    )
    .await
    {
        Ok(c) => c,
        Err(Error::MissingDependency(msg)) => {
            let existing = restore
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
                restore.metadata.generation,
            );
            io::patch_status(
                api,
                name,
                serde_json::json!({ "phase": "Pending", "conditions": conditions }),
            )
            .await?;
            io::publish_missing_creds_event(ctx, restore, &msg).await;
            return Err(Error::MissingDependency(msg));
        }
        Err(e) => return Err(e),
    };
    if creds.projected > 0 {
        ctx.metrics
            .inc_secrets_projected(namespace, creds.projected);
    }
    // Creds present (or projected): clear any stale `CredentialsAvailable=False`.
    if let Some(conds) = restore.status.as_ref().map(|s| s.conditions.as_slice())
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
            restore.metadata.generation,
        );
        io::patch_status(api, name, serde_json::json!({ "conditions": conditions })).await?;
    }
    let creds_secrets = creds.names;

    let identity = source
        .identity
        .as_ref()
        .and_then(|i| {
            i.source_path.as_ref().map(|source_path| MoverIdentity {
                username: i.username.clone(),
                hostname: i.hostname.clone(),
                source_path: source_path.clone(),
            })
        })
        .unwrap_or_else(|| MoverIdentity {
            username: "restore".into(),
            hostname: namespace.to_string(),
            source_path: target_path.clone(),
        });
    // Carry the Restore CRD's options (ADR §4.6) through to the mover so kopia
    // honors them. `None` lets kopia use its defaults.
    let (ignore_permission_errors, write_files_atomically) = restore
        .spec
        .options
        .as_ref()
        .map(|o| (o.ignore_permission_errors, o.write_files_atomically))
        .unwrap_or((None, None));
    // Effective cache config (repository cacheDefaults overlaid by this restore's
    // mover.cache, ADR §3.1) drives both the connect budgets and the cache volume.
    let effective_cache = crate::cache::effective_cache(
        &repo,
        restore.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
    );
    let cache = crate::cache::cache_tuning(effective_cache.as_ref());
    let work_spec = MoverWorkSpec {
        version: 1,
        operation: Operation::Restore(RestoreOp {
            snapshot_id: source.kopia_snapshot_id.clone(),
            target_path: target_path.clone(),
            ignore_permission_errors,
            write_files_atomically,
        }),
        identity,
        repository: restore_connect(&repo)?,
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            kind: "Restore".to_string(),
            name: name.to_string(),
            namespace: namespace.to_string(),
        },
        hook_plan: Default::default(),
        options: MoverOptions::default(),
        cache,
        // Repo throttle applies to restore too (§13(e)).
        throttle: io::throttle_spec(repo.mover_defaults.as_ref()),
    };
    let repo_volume =
        io::filesystem_repo_mount_source(&repo.backend).map(|source| VolumeMountSpec {
            source,
            mount_path: io::filesystem_repo_path(&repo.backend).unwrap_or_default(),
            read_only: true,
        });
    // Resolve the cache VOLUME; a persistent cache PVC is owned by this Restore.
    let cache_volume = crate::cache::resolve_cache_volume(
        &ctx.client,
        namespace,
        owner.clone(),
        &format!("kopiur-cache-{job_name}"),
        effective_cache.as_ref(),
    )
    .await?;
    // RWO Multi-Attach avoidance for the restore DESTINATION PVC: when restoring into
    // an existing ReadWriteOnce PVC held by a running app pod, pin the restore mover to
    // that node so the kubelet can attach the volume (a freshly-created `target.pvc`
    // has no holder → no pin). The resolved `sourceColocation` mode (default `Auto`)
    // decides. RWO multi-attach fix.
    let (mover_affinity, mover_tolerations) = {
        let decision = io::resolve_source_colocation(
            &ctx.client,
            namespace,
            target_pvc,
            resolved_mover.source_colocation,
        )
        .await?;
        io::apply_colocation(
            decision,
            resolved_mover.affinity.clone(),
            resolved_mover.tolerations.clone(),
        )?
    };
    let inputs = MoverJobInputs {
        name: job_name,
        namespace,
        owner,
        work_spec: &work_spec,
        image: &ctx.mover_image,
        image_pull_policy: crate::snapshot::mover_pull_policy_pub(),
        limits: {
            let mut l = restore_job_limits(restore);
            if l.ttl_seconds_after_finished.is_none() {
                l.ttl_seconds_after_finished = resolved_mover.ttl_seconds_after_finished;
            }
            l
        },
        resources: resolved_mover.resources.clone(),
        // The fully-merged contexts (hardened ⊂ moverDefaults ⊂ recipe) — the same
        // values the privileged gate above ran on.
        security_context: resolved_mover.security_context.clone(),
        pod_security_context: resolved_mover.pod_security_context.clone(),
        node_selector: resolved_mover.node_selector.clone(),
        tolerations: mover_tolerations,
        affinity: mover_affinity,
        labels: {
            let mut labels =
                io::child_labels(&[(crate::consts::OP_LABEL, crate::consts::OP_RESTORE)]);
            mover_identity.decorate_labels(&mut labels);
            labels
        },
        // Restore writes INTO the target PVC, mounted read-write at /restore.
        source_volume: Some(VolumeMountSpec::pvc(target_pvc, target_path, false)),
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
    io::apply_mover_objects(&ctx.client, namespace, job_name, &cm, &job).await?;
    tracing::info!(restore = %name, job = %job_name, snapshot_id = %source.kopia_snapshot_id, "created restore mover Job");
    // The Job is new; the CALLER writes the matching status (direct: MoverJobCreated;
    // populator: PopulatingPrimePvc) so each path keeps its own phase discipline.
    Ok(MoverOutcome::Running { created: true })
}

/// A fully-resolved restore source, ready to pin to `status.resolved` (ADR §4.6):
/// the exact kopia snapshot id plus its provenance (the `Snapshot` CR or the
/// kopia identity it was selected by).
#[derive(Debug, Clone)]
struct ResolvedSource {
    kopia_snapshot_id: String,
    snapshot_ref: Option<kopiur_api::common::ObjectRef>,
    identity: Option<kopiur_api::common::ResolvedIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PolicySnapshotCandidate {
    kopia_snapshot_id: String,
    identity: kopiur_api::common::ResolvedIdentity,
    end_time: chrono::DateTime<chrono::Utc>,
}

/// Succeeded `Snapshot` CRs are the controller's cache of concrete kopia
/// manifest ids. Prefer them for `fromPolicy` resolution so filesystem
/// `ClusterRepository` restores do not require the repository path (for example
/// `/repo`) to be mounted into the controller pod; the mover job owns that mount.
fn policy_snapshot_candidates_from_crs(
    snapshots: Vec<Snapshot>,
    policy_name: &str,
    policy_namespace: &str,
    identity: &kopiur_api::common::ResolvedIdentity,
) -> Vec<PolicySnapshotCandidate> {
    let mut out: Vec<_> = snapshots
        .into_iter()
        .filter_map(|snap| {
            let snap_ns = snap
                .namespace()
                .unwrap_or_else(|| policy_namespace.to_string());
            let pref = snap.spec.policy_ref.as_ref()?;
            let pref_ns = pref.namespace.as_deref().unwrap_or(&snap_ns);
            if pref.name != policy_name || pref_ns != policy_namespace {
                return None;
            }
            let status = snap.status.as_ref()?;
            if status.phase != Some(SnapshotPhase::Succeeded) {
                return None;
            }
            let info = status.snapshot.as_ref()?;
            if &info.identity != identity {
                return None;
            }
            let end_time = status
                .timing
                .as_ref()
                .and_then(|t| t.end_time.as_deref())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&chrono::Utc))
                .or_else(|| {
                    snap.metadata.creation_timestamp.as_ref().and_then(|t| {
                        chrono::DateTime::<chrono::Utc>::from_timestamp(t.0.as_second(), 0)
                    })
                })?;
            Some(PolicySnapshotCandidate {
                kopia_snapshot_id: info.kopia_snapshot_id.clone(),
                identity: info.identity.clone(),
                end_time,
            })
        })
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.end_time));
    out
}

fn filter_policy_snapshot_candidates_as_of(
    mut snapshots: Vec<PolicySnapshotCandidate>,
    as_of: Option<&str>,
) -> Result<Vec<PolicySnapshotCandidate>> {
    let Some(s) = as_of else {
        return Ok(snapshots);
    };
    let cutoff = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|e| {
            Error::Validation(format!(
                "source asOf {s:?} is not an RFC3339 timestamp; use e.g. \
                 2026-05-01T00:00:00Z (the newest snapshot at or before this instant \
                 is restored): {e}"
            ))
        })?
        .with_timezone(&chrono::Utc);
    snapshots.retain(|e| e.end_time <= cutoff);
    Ok(snapshots)
}

fn pick_policy_snapshot_candidate(
    snapshots: Vec<PolicySnapshotCandidate>,
    offset: i64,
) -> Option<PolicySnapshotCandidate> {
    let idx = offset.max(0) as usize;
    snapshots.into_iter().nth(idx)
}

async fn list_policy_snapshot_cr_candidates(
    ctx: &Context,
    namespace: &str,
    policy_name: &str,
    identity: &kopiur_api::common::ResolvedIdentity,
) -> Result<Vec<PolicySnapshotCandidate>> {
    let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), namespace);
    let snapshots = api.list(&ListParams::default()).await?.items;
    Ok(policy_snapshot_candidates_from_crs(
        snapshots,
        policy_name,
        namespace,
        identity,
    ))
}

/// Best-effort, **positive-only** restore-direction securityContext check. If a pod already
/// consumes the target PVC `claim` and the mover's *write* identity provably matches it (same
/// UID, or a matching `fsGroup` on the fresh volume), records
/// `RestoreSecurityContextCompatible=True`. It NEVER writes `False` or emits an Event: a
/// restore has no certain runtime signal (the future workload may not exist yet, and a
/// mismatch isn't proof the data will be unreadable), so a heuristic negative would be an
/// un-retractable false alarm. The advisory negative lives in the admission warning. Never
/// returns an error.
async fn assess_restore_security_context(
    namespace: &str,
    restore: &Restore,
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
        Err(e) => {
            tracing::debug!(error = %e, %namespace, "restore securityContext compat: pod list failed; skipping");
            return;
        }
    };
    // The future consumer: a non-kopiur workload already mounting the target PVC (often none).
    let consumer = kopiur_api::secctx_compat::workload_identities(&pods, claim)
        .into_iter()
        .next();

    let mover = kopiur_api::secctx_compat::mover_write_identity(sc, psc);
    let kopiur_api::secctx_compat::RestoreWriteCompat::Compatible { .. } =
        kopiur_api::secctx_compat::assess_restore_compat(&mover, consumer.as_ref())
    else {
        // Absent consumer / undecidable / heuristic mismatch → stay silent (the admission
        // warning carries the advisory heads-up; there is no certain signal to assert here).
        return;
    };

    let existing = restore
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    let conditions = io::upsert_condition(
        &existing,
        RESTORE_SECURITY_CONTEXT_COMPATIBLE_CONDITION,
        true,
        SECURITY_CONTEXT_COMPATIBLE_REASON,
        "the future workload consuming the target PVC can read what the mover writes (matching \
         UID, or a shared fsGroup on the fresh volume)",
        restore.metadata.generation,
    );
    let name = restore.name_any();
    let api: Api<Restore> = Api::namespaced(ctx.client.clone(), namespace);
    let current = serde_json::to_value(&restore.status).ok();
    if let Err(e) = io::patch_status_if_changed(
        &api,
        &name,
        current.as_ref(),
        serde_json::json!({ "conditions": conditions }),
    )
    .await
    {
        tracing::debug!(error = %e, %name, "restore securityContext compat: condition patch failed");
    }
}

/// Create the `target.pvc` PVC if it doesn't exist (idempotent). Deliberately
/// NOT owner-referenced to the `Restore`: the restored data must survive
/// `kubectl delete restore` — GC'ing the target PVC with the CR would destroy
/// what the user just recovered. Missing `capacity` is rejected (webhook + here,
/// defensively): a silently-defaulted size could truncate the restored data.
async fn ensure_restore_target_pvc(
    ctx: &Context,
    namespace: &str,
    template: &kopiur_api::restore::PvcTemplate,
) -> Result<()> {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
    if pvc_api.get_opt(&template.name).await?.is_some() {
        return Ok(());
    }
    let capacity = template.capacity.as_deref().ok_or_else(|| {
        Error::Validation(format!(
            "restore target.pvc {:?} has no capacity; set target.pvc.capacity (e.g. 10Gi, at \
             least the size of the data being restored) — the operator will not guess a size \
             for a PVC it creates",
            template.name
        ))
    })?;
    let access_modes = if template.access_modes.is_empty() {
        vec!["ReadWriteOnce".to_string()]
    } else {
        template.access_modes.clone()
    };
    let pvc: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": template.name,
            "namespace": namespace,
            "labels": io::child_labels(&[(crate::consts::OP_LABEL, crate::consts::OP_RESTORE_TARGET)]),
        },
        "spec": {
            "accessModes": access_modes,
            "resources": { "requests": { "storage": capacity } },
            "storageClassName": template.storage_class_name,
        },
    }))?;
    match pvc_api
        .create(&kube::api::PostParams::default(), &pvc)
        .await
    {
        Ok(_) => {
            tracing::info!(pvc = %template.name, %namespace, "created restore target PVC");
            Ok(())
        }
        // Lost a create race with another reconcile — the PVC exists, which is
        // all this function guarantees.
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Resolve the restore's source to a concrete kopia snapshot. Returns `None`
/// when no snapshot matches (caller applies `waitTimeout` + `onMissingSnapshot`).
async fn resolve_snapshot(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
) -> Result<Option<ResolvedSource>> {
    use kopiur_api::common::{ObjectRef, ResolvedIdentity};
    match &restore.spec.source {
        RestoreSource::SnapshotRef(r) => {
            let ns = r.namespace.as_deref().unwrap_or(namespace);
            let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), ns);
            let backup = api.get_opt(&r.name).await?;
            Ok(backup
                .and_then(|b| b.status)
                .and_then(|s| s.snapshot)
                .map(|s| ResolvedSource {
                    kopia_snapshot_id: s.kopia_snapshot_id,
                    snapshot_ref: Some(ObjectRef {
                        name: r.name.clone(),
                        namespace: Some(ns.to_string()),
                    }),
                    identity: Some(s.identity),
                }))
        }
        RestoreSource::Identity(id) => {
            let identity = ResolvedIdentity {
                username: id.username.clone(),
                hostname: id.hostname.clone(),
                source_path: id.source_path.clone(),
            };
            // An explicit snapshot id wins; otherwise resolve via snapshot list.
            if let Some(sid) = &id.snapshot_id {
                return Ok(Some(ResolvedSource {
                    kopia_snapshot_id: sid.clone(),
                    snapshot_ref: None,
                    identity: Some(identity),
                }));
            }
            let repo = resolve_restore_repository(ctx, restore, namespace).await?;
            let snapshots = list_for_identity(
                ctx,
                &repo,
                namespace,
                &id.username,
                &id.hostname,
                id.source_path.as_deref(),
            )
            .await?;
            let snapshots = filter_as_of(snapshots, id.as_of.as_deref())?;
            Ok(
                pick_offset(snapshots, id.offset.unwrap_or(0)).map(|sid| ResolvedSource {
                    kopia_snapshot_id: sid,
                    snapshot_ref: None,
                    identity: Some(identity),
                }),
            )
        }
        RestoreSource::FromPolicy(c) => {
            // Resolve identity from the SnapshotPolicy, then list newest/offset.
            use kopiur_api::SnapshotPolicy;
            let cfg_ns = c.namespace.as_deref().unwrap_or(namespace);
            let cfg_api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), cfg_ns);
            let config = cfg_api.get_opt(&c.name).await?.ok_or_else(|| {
                Error::MissingDependency(format!("SnapshotPolicy {cfg_ns}/{}", c.name))
            })?;
            let repo = resolve_restore_repository(ctx, restore, namespace).await?;
            let identity = crate::snapshot_policy::config_identity(
                &config,
                cfg_ns,
                repo.identity_defaults.as_ref(),
            )?;
            let cr_snapshots =
                list_policy_snapshot_cr_candidates(ctx, cfg_ns, &c.name, &identity).await?;
            let cr_snapshots =
                filter_policy_snapshot_candidates_as_of(cr_snapshots, c.as_of.as_deref())?;
            if let Some(candidate) = pick_policy_snapshot_candidate(cr_snapshots, c.offset) {
                return Ok(Some(ResolvedSource {
                    kopia_snapshot_id: candidate.kopia_snapshot_id,
                    snapshot_ref: None,
                    identity: Some(candidate.identity),
                }));
            }

            let snapshots = list_for_identity(
                ctx,
                &repo,
                namespace,
                &identity.username,
                &identity.hostname,
                identity.source_path.as_deref(),
            )
            .await?;
            let snapshots = filter_as_of(snapshots, c.as_of.as_deref())?;
            Ok(pick_offset(snapshots, c.offset).map(|sid| ResolvedSource {
                kopia_snapshot_id: sid,
                snapshot_ref: None,
                identity: Some(identity),
            }))
        }
    }
}

/// Keep only snapshots taken at or before `asOf` (point-in-time selection,
/// applied BEFORE `offset` so the two compose: "the previous one as of last
/// Tuesday"). `None` keeps the full list. The webhook validates the format at
/// admission; re-parsing here is defensive (one validator, two callers).
fn filter_as_of(
    mut snapshots: Vec<kopiur_kopia::SnapshotListEntry>,
    as_of: Option<&str>,
) -> Result<Vec<kopiur_kopia::SnapshotListEntry>> {
    let Some(s) = as_of else {
        return Ok(snapshots);
    };
    let cutoff = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|e| {
            Error::Validation(format!(
                "source asOf {s:?} is not an RFC3339 timestamp; use e.g. \
                 2026-05-01T00:00:00Z (the newest snapshot at or before this instant \
                 is restored): {e}"
            ))
        })?
        .with_timezone(&chrono::Utc);
    snapshots.retain(|e| e.end_time <= cutoff);
    Ok(snapshots)
}

/// Seconds left in the `waitTimeout` window that started at the Restore's
/// creation, or `None` when no (parseable) window is configured or it has
/// elapsed. Pure, clock-free — unit-tested without a cluster.
pub fn wait_remaining_secs(
    created_epoch: i64,
    wait_timeout: Option<&str>,
    now_epoch: i64,
) -> Option<u64> {
    let timeout = crate::snapshot_schedule::parse_go_duration(wait_timeout?)?;
    let deadline = created_epoch.saturating_add(timeout.as_secs().try_into().ok()?);
    (now_epoch < deadline).then(|| (deadline - now_epoch) as u64)
}

/// kopia snapshot list filtered to one identity (filesystem in-process path),
/// newest-first.
async fn list_for_identity(
    ctx: &Context,
    repo: &ResolvedRepository,
    namespace: &str,
    username: &str,
    hostname: &str,
    source_path: Option<&str>,
) -> Result<Vec<kopiur_kopia::SnapshotListEntry>> {
    use kopiur_api::backend::Backend;
    let creds = io::repo_credentials(&repo.encryption);
    match &repo.backend {
        Backend::Filesystem(fs) => {
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
            let filter = kopiur_kopia::SnapshotSource {
                host: hostname.to_string(),
                user_name: username.to_string(),
                path: source_path.unwrap_or("").to_string(),
            };
            let mut list = client.snapshot_list(Some(&filter)).await?;
            list.sort_by_key(|e| std::cmp::Reverse(e.end_time));
            Ok(list)
        }
        // In-process snapshot listing needs a locally mounted repo; object-store
        // backends cannot be listed here. Fail LOUDLY with the fix (snapshotRef or
        // a pinned snapshotID) instead of returning an empty list that would read
        // as "no snapshots" and silently Continue/Fail. Exhaustive so a new
        // backend must decide its resolution story before it compiles.
        b @ (Backend::S3(_)
        | Backend::Azure(_)
        | Backend::Gcs(_)
        | Backend::B2(_)
        | Backend::Sftp(_)
        | Backend::WebDav(_)
        | Backend::Rclone(_)) => Err(Error::UnsupportedSourceResolution {
            backend: b.kind_str(),
        }),
    }
}

/// Pick the snapshot at `offset` (0 = newest) from a newest-first list.
fn pick_offset(snapshots: Vec<kopiur_kopia::SnapshotListEntry>, offset: i64) -> Option<String> {
    let idx = offset.max(0) as usize;
    snapshots.into_iter().nth(idx).map(|e| e.id)
}

/// Derive the repository a `Snapshot` belongs to, for `Restore.spec.repository`
/// derivation (the CRD documents `repository` as derived-from-source for
/// `snapshotRef`). The pure rule lives in the api crate
/// ([`kopiur_api::snapshot::repository_ref_for`], with its tests) because the
/// `kubectl kopiur` browse data-plane shares it; re-exported here for
/// controller callers.
pub(crate) use kopiur_api::snapshot::repository_ref_for as repository_ref_from_snapshot;

/// Resolve the repository a restore targets: explicit `spec.repository`, or
/// derived from the source — the snapshotRef'd Snapshot's pinned/owning
/// repository, or the fromPolicy policy's repository. Only `source.identity`
/// has nothing to derive from and requires the explicit field.
async fn resolve_restore_repository(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
) -> Result<ResolvedRepository> {
    // Explicit `spec.repository` wins. Honors `kind` (namespaced vs.
    // ClusterRepository) via the shared resolver (ADR §5.5).
    if let Some(rref) = &restore.spec.repository {
        return io::resolve_repository_ref(&ctx.client, rref, namespace).await;
    }
    // SnapshotRef: derive from the referenced Snapshot (pinned resolved
    // repository for produced, owning repository for discovered).
    if let RestoreSource::SnapshotRef(sref) = &restore.spec.source {
        let snap_ns = sref.namespace.as_deref().unwrap_or(namespace);
        let snap_api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), snap_ns);
        let snap = snap_api
            .get_opt(&sref.name)
            .await?
            .ok_or_else(|| Error::MissingDependency(format!("Snapshot {snap_ns}/{}", sref.name)))?;
        let rref = repository_ref_from_snapshot(&snap).ok_or_else(|| {
            Error::Validation(format!(
                "cannot derive the repository from Snapshot {snap_ns}/{}: it has neither a \
                 pinned status.resolved.repository nor a Repository/ClusterRepository owner; \
                 set restore.spec.repository explicitly",
                sref.name
            ))
        })?;
        // Resolved relative to the SNAPSHOT's namespace (an absent ref
        // namespace means "same as the snapshot", not "same as the restore").
        return io::resolve_repository_ref(&ctx.client, &rref, snap_ns).await;
    }
    // FromPolicy: resolve via the SnapshotPolicy's repository.
    if let RestoreSource::FromPolicy(c) = &restore.spec.source {
        use kopiur_api::SnapshotPolicy;
        let cfg_ns = c.namespace.as_deref().unwrap_or(namespace);
        let cfg_api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), cfg_ns);
        let config = cfg_api.get_opt(&c.name).await?.ok_or_else(|| {
            Error::MissingDependency(format!("SnapshotPolicy {cfg_ns}/{}", c.name))
        })?;
        return io::resolve_repository_ref(&ctx.client, &config.spec.repository, cfg_ns).await;
    }
    Err(Error::Validation(
        "restore with source.identity requires spec.repository (snapshotRef and fromPolicy \
         sources derive it; a raw identity has nothing to derive from)"
            .into(),
    ))
}

/// Map a resolved repository backend to the mover connect spec for a restore.
fn restore_connect(repo: &ResolvedRepository) -> Result<RepositoryConnect> {
    crate::snapshot::repository_connect_pub(repo)
}

/// Mover `Job` limits from the restore's `failurePolicy`, falling back to ADR
/// defaults. Mirrors `snapshot::job_limits`; TTL stays unset so the one-Job-per-CR is
/// reaped by owner-reference GC when the `Restore` is deleted.
fn restore_job_limits(restore: &Restore) -> JobLimits {
    match &restore.spec.failure_policy {
        Some(fp) => JobLimits {
            backoff_limit: fp.backoff_limit.unwrap_or(2),
            active_deadline_seconds: fp.active_deadline_seconds,
            ..JobLimits::default()
        },
        None => JobLimits::default(),
    }
}

/// `error_policy` for the `Restore` controller.
pub fn error_policy(obj: Arc<Restore>, err: &Error, ctx: Arc<Context>) -> Action {
    error_policy_for("Restore", obj.as_ref(), err, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::common::ObjectRef;
    use kopiur_api::restore::{FromPolicy, IdentitySource};

    // The repository-derivation tests moved to `kopiur_api::snapshot` with the
    // pure fn (`repository_ref_for`); the browse data-plane shares it.

    fn job_with_times(start: Option<&str>, end: Option<&str>) -> k8s_openapi::api::batch::v1::Job {
        use k8s_openapi::api::batch::v1::{Job, JobStatus};
        let parse = |s: &str| serde_json::from_value(serde_json::json!(s)).unwrap();
        Job {
            status: Some(JobStatus {
                start_time: start.map(parse),
                completion_time: end.map(parse),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn restore_duration_is_completion_minus_start() {
        let job = job_with_times(Some("2024-01-01T00:00:00Z"), Some("2024-01-01T00:01:30Z"));
        assert_eq!(restore_job_duration_seconds(&job), Some(90));
        // Missing completion → None (still running).
        assert_eq!(
            restore_job_duration_seconds(&job_with_times(Some("2024-01-01T00:00:00Z"), None)),
            None
        );
        // Negative interval (clock skew) → None.
        let skew = job_with_times(Some("2024-01-01T00:01:00Z"), Some("2024-01-01T00:00:00Z"));
        assert_eq!(restore_job_duration_seconds(&skew), None);
    }

    fn snapshot_ref() -> RestoreSource {
        RestoreSource::SnapshotRef(ObjectRef {
            name: "b".into(),
            namespace: None,
        })
    }
    fn from_config() -> RestoreSource {
        RestoreSource::FromPolicy(FromPolicy {
            name: "cfg".into(),
            namespace: None,
            as_of: None,
            offset: 0,
        })
    }
    fn identity() -> RestoreSource {
        RestoreSource::Identity(IdentitySource {
            username: "u".into(),
            hostname: "h".into(),
            source_path: None,
            snapshot_id: None,
            as_of: None,
            offset: None,
        })
    }

    #[test]
    fn from_config_defaults_to_continue_others_fail() {
        assert_eq!(
            default_on_missing(&from_config()),
            OnMissingSnapshot::Continue
        );
        assert_eq!(default_on_missing(&snapshot_ref()), OnMissingSnapshot::Fail);
        assert_eq!(default_on_missing(&identity()), OnMissingSnapshot::Fail);
    }

    #[test]
    fn explicit_on_missing_overrides_default() {
        // fromPolicy would default Continue, but an explicit Fail wins.
        assert_eq!(
            effective_on_missing(Some(OnMissingSnapshot::Fail), &from_config()),
            OnMissingSnapshot::Fail
        );
        // snapshotRef defaults Fail, explicit Continue wins.
        assert_eq!(
            effective_on_missing(Some(OnMissingSnapshot::Continue), &snapshot_ref()),
            OnMissingSnapshot::Continue
        );
    }

    #[test]
    fn source_mode_strings_match_each_variant() {
        assert_eq!(source_mode(&snapshot_ref()), "SnapshotRef");
        assert_eq!(source_mode(&from_config()), "FromPolicy");
        assert_eq!(source_mode(&identity()), "Identity");
    }

    fn list_entry(id: &str, end_time: &str) -> kopiur_kopia::SnapshotListEntry {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "source": { "host": "h", "userName": "u", "path": "/data" },
            "startTime": end_time,
            "endTime": end_time,
        }))
        .expect("valid SnapshotListEntry")
    }

    /// Three snapshots, newest-first (the order `list_for_identity` returns).
    fn three_snapshots() -> Vec<kopiur_kopia::SnapshotListEntry> {
        vec![
            list_entry("k3", "2026-06-03T00:00:00Z"),
            list_entry("k2", "2026-06-02T00:00:00Z"),
            list_entry("k1", "2026-06-01T00:00:00Z"),
        ]
    }

    fn policy_snapshot(name: &str, id: &str, end_time: &str) -> Snapshot {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Snapshot",
            "metadata": { "name": name, "namespace": "ns", "creationTimestamp": end_time },
            "spec": { "policyRef": { "name": "cfg" } },
            "status": {
                "phase": "Succeeded",
                "snapshot": {
                    "kopiaSnapshotID": id,
                    "identity": { "username": "u", "hostname": "h", "sourcePath": "/data" }
                },
                "timing": { "endTime": end_time }
            }
        }))
        .expect("valid Snapshot")
    }

    #[test]
    fn from_policy_prefers_succeeded_snapshot_cr_manifest_ids() {
        let identity = kopiur_api::common::ResolvedIdentity {
            username: "u".into(),
            hostname: "h".into(),
            source_path: Some("/data".into()),
        };
        let mut wrong_policy = policy_snapshot("other", "ignored-policy", "2026-06-04T00:00:00Z");
        wrong_policy.spec.policy_ref.as_mut().unwrap().name = "other".into();
        let mut wrong_identity =
            policy_snapshot("wrong-id", "ignored-identity", "2026-06-05T00:00:00Z");
        wrong_identity
            .status
            .as_mut()
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .identity
            .source_path = Some("/other".into());

        let picked = policy_snapshot_candidates_from_crs(
            vec![
                policy_snapshot("old", "manifest-old", "2026-06-01T00:00:00Z"),
                policy_snapshot("new", "manifest-new", "2026-06-03T00:00:00Z"),
                wrong_policy,
                wrong_identity,
            ],
            "cfg",
            "ns",
            &identity,
        );

        assert_eq!(
            picked
                .iter()
                .map(|e| e.kopia_snapshot_id.as_str())
                .collect::<Vec<_>>(),
            ["manifest-new", "manifest-old"]
        );
        let kept =
            filter_policy_snapshot_candidates_as_of(picked, Some("2026-06-02T00:00:00Z")).unwrap();
        assert_eq!(
            pick_policy_snapshot_candidate(kept, 0).map(|e| e.kopia_snapshot_id),
            Some("manifest-old".to_string())
        );
    }

    #[test]
    fn filter_as_of_keeps_snapshots_at_or_before_the_instant() {
        // A cutoff between k2 and k3 drops k3 (newer than the instant); k2/k1 remain.
        let kept = filter_as_of(three_snapshots(), Some("2026-06-02T12:00:00Z")).unwrap();
        assert_eq!(
            kept.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["k2", "k1"]
        );
        // Exactly AT a snapshot's endTime keeps it ("at or before").
        let kept = filter_as_of(three_snapshots(), Some("2026-06-02T00:00:00Z")).unwrap();
        assert_eq!(kept.first().map(|e| e.id.as_str()), Some("k2"));
        // Before everything → empty (caller applies onMissingSnapshot).
        let kept = filter_as_of(three_snapshots(), Some("2026-05-01T00:00:00Z")).unwrap();
        assert!(kept.is_empty());
        // No asOf → untouched.
        let kept = filter_as_of(three_snapshots(), None).unwrap();
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn filter_as_of_rejects_non_rfc3339_with_actionable_message() {
        let err = filter_as_of(three_snapshots(), Some("yesterday")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("yesterday"), "{msg}");
        assert!(msg.contains("RFC3339"), "{msg}");
        assert!(msg.contains("2026-05-01T00:00:00Z"), "{msg}");
    }

    #[test]
    fn as_of_composes_with_offset() {
        // "the previous one as of just after k2": asOf drops k3, offset 1 then
        // steps past k2 to k1.
        let kept = filter_as_of(three_snapshots(), Some("2026-06-02T12:00:00Z")).unwrap();
        assert_eq!(pick_offset(kept, 1), Some("k1".to_string()));
    }

    #[test]
    fn pick_offset_zero_is_newest_and_out_of_range_is_none() {
        assert_eq!(pick_offset(three_snapshots(), 0), Some("k3".to_string()));
        assert_eq!(pick_offset(three_snapshots(), 2), Some("k1".to_string()));
        assert_eq!(pick_offset(three_snapshots(), 3), None);
        // A negative offset clamps to newest rather than panicking.
        assert_eq!(pick_offset(three_snapshots(), -1), Some("k3".to_string()));
    }

    #[test]
    fn wait_remaining_counts_down_from_creation_and_closes() {
        // 5m window, 60s elapsed → 240s left.
        assert_eq!(wait_remaining_secs(1000, Some("5m"), 1060), Some(240));
        // Window exactly elapsed → closed (None), onMissingSnapshot applies.
        assert_eq!(wait_remaining_secs(1000, Some("5m"), 1300), None);
        assert_eq!(wait_remaining_secs(1000, Some("5m"), 1301), None);
        // No waitTimeout configured → no window at all.
        assert_eq!(wait_remaining_secs(1000, None, 1000), None);
        // Unparseable timeout → treated as no window (webhook rejects it at
        // admission; this is the defensive path).
        assert_eq!(wait_remaining_secs(1000, Some("bogus"), 1000), None);
    }

    #[test]
    fn populator_state_depends_on_target_variant() {
        use kopiur_api::PopulatorTarget;
        use kopiur_api::common::ObjectRef;
        use kopiur_api::restore::PvcTemplate;
        // populator target → passive AwaitingClaim.
        assert_eq!(
            populator_state(&RestoreTarget::Populator(PopulatorTarget {})),
            PopulatorState::AwaitingClaim
        );
        // explicit pvc/pvcRef → operator-driven DirectTarget.
        assert_eq!(
            populator_state(&RestoreTarget::PvcRef(ObjectRef {
                name: "data".into(),
                namespace: None,
            })),
            PopulatorState::DirectTarget
        );
        assert_eq!(
            populator_state(&RestoreTarget::Pvc(PvcTemplate {
                name: "created".into(),
                storage_class_name: None,
                capacity: None,
                access_modes: vec![],
            })),
            PopulatorState::DirectTarget
        );
    }

    #[test]
    fn populator_completed_is_not_terminal_at_guard() {
        use PopulatorState::{AwaitingClaim, DirectTarget};
        use RestorePhase::{Completed, Failed, Pending, Resolving, Restoring};

        // A populator `Completed` (mover done with the prime PVC, rebind still pending)
        // must NOT be terminal at the guard, or the rebind never runs.
        assert!(!phase_is_terminal_at_guard(Completed, AwaitingClaim));
        // A direct restore writes the target itself, so `Completed` IS terminal.
        assert!(phase_is_terminal_at_guard(Completed, DirectTarget));
        // `Failed` is terminal regardless of dispatch model.
        assert!(phase_is_terminal_at_guard(Failed, AwaitingClaim));
        assert!(phase_is_terminal_at_guard(Failed, DirectTarget));
        // In-flight phases are never terminal.
        for p in [Pending, Resolving, Restoring] {
            assert!(!phase_is_terminal_at_guard(p, AwaitingClaim));
            assert!(!phase_is_terminal_at_guard(p, DirectTarget));
        }
    }

    // --- kstatus Ready conditions (ADR-0005 §2) -----------------------------
    // Regression: the job-terminal transitions used to write the phase ALONE
    // (no conditions), so `kubectl wait --for=condition=Ready` and Flux
    // healthChecks could never gate on a Completed Restore; and the
    // missing-snapshot/awaiting-claim patches replaced the whole conditions
    // array, dropping domain conditions set earlier.

    #[test]
    fn ready_outcome_maps_every_phase() {
        use crate::io::ReadyOutcome;
        assert_eq!(
            restore_ready_outcome(RestorePhase::Completed),
            ReadyOutcome::Ready
        );
        assert_eq!(
            restore_ready_outcome(RestorePhase::Failed),
            ReadyOutcome::Stalled
        );
        for p in [
            RestorePhase::Pending,
            RestorePhase::Resolving,
            RestorePhase::Restoring,
        ] {
            assert_eq!(restore_ready_outcome(p), ReadyOutcome::Reconciling, "{p:?}");
        }
    }

    /// A minimal Restore with `generation: 3` and one pre-existing condition,
    /// parsed the cluster's way (JSON → typed).
    fn restore_with_condition(type_: &str, status: &str) -> Restore {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Restore",
            "metadata": { "name": "r", "namespace": "ns", "generation": 3 },
            "spec": {
                "source": { "snapshotRef": { "name": "b" } },
                "target": { "pvcRef": { "name": "t" } }
            },
            "status": { "conditions": [{
                "type": type_, "status": status, "reason": "X", "message": "m",
                "lastTransitionTime": "2026-01-01T00:00:00Z"
            }] }
        }))
        .expect("valid Restore")
    }

    fn cond<'a>(v: &'a serde_json::Value, type_: &str) -> &'a serde_json::Value {
        v["conditions"]
            .as_array()
            .expect("conditions array")
            .iter()
            .find(|c| c["type"] == type_)
            .unwrap_or_else(|| panic!("missing condition {type_}"))
    }

    #[test]
    fn ready_status_completed_sets_ready_and_preserves_domain_conditions() {
        let r = restore_with_condition("Resolved", "True");
        let v = restore_ready_status(&r, RestorePhase::Completed, "RestoreSucceeded", "done");
        assert_eq!(v["phase"], "Completed");
        assert_eq!(v["observedGeneration"], 3);
        assert_eq!(cond(&v, "Ready")["status"], "True");
        assert_eq!(cond(&v, "Ready")["reason"], "RestoreSucceeded");
        assert_eq!(cond(&v, "Reconciling")["status"], "False");
        assert_eq!(cond(&v, "Stalled")["status"], "False");
        // The pre-existing domain condition survives the phase write (the old
        // bare-array patches dropped it).
        assert_eq!(cond(&v, "Resolved")["status"], "True");
    }

    #[test]
    fn ready_status_failed_is_stalled_not_ready() {
        let r = restore_with_condition("MoverPermitted", "True");
        let v = restore_ready_status(
            &r,
            RestorePhase::Failed,
            "MoverJobFailed",
            "the restore mover Job failed",
        );
        assert_eq!(v["phase"], "Failed");
        assert_eq!(cond(&v, "Ready")["status"], "False");
        assert_eq!(cond(&v, "Stalled")["status"], "True");
        assert_eq!(cond(&v, "Stalled")["reason"], "MoverJobFailed");
        assert_eq!(cond(&v, "MoverPermitted")["status"], "True");
    }

    /// The mover-stamp race the e2e caught live: the mover PATCHes
    /// `phase: Completed` (no conditions) before the controller's Job-terminal
    /// transition runs, so the object sits terminal with the in-flight trio
    /// (`Ready=False reason=MoverJobCreated`). The terminal gate must detect
    /// that as NOT settled and heal; once healed it must read as settled (the
    /// self-gate that stops re-patching).
    #[test]
    fn mover_stamped_terminal_phase_without_ready_is_not_settled() {
        let mut r = restore_with_condition("Resolved", "True");
        // In-flight trio, as written by the MoverJobCreated transition.
        let inflight = io::set_ready(
            &r.status.as_ref().unwrap().conditions,
            r.metadata.generation,
            io::ReadyOutcome::Reconciling,
            "MoverJobCreated",
            "created the restore mover Job",
        );
        let mut status = r.status.take().unwrap();
        status.conditions = inflight;
        status.phase = Some(RestorePhase::Completed); // mover stamp: phase only
        r.status = Some(status);

        assert!(!kstatus_settled_for(&r, RestorePhase::Completed));
        assert!(!kstatus_settled_for(&r, RestorePhase::Failed));

        // Heal (what the terminal gate patches), then it must be settled.
        let healed = restore_ready_status(&r, RestorePhase::Completed, "RestoreSucceeded", "done");
        let mut status = r.status.take().unwrap();
        status.conditions = serde_json::from_value(healed["conditions"].clone()).unwrap();
        r.status = Some(status);
        assert!(kstatus_settled_for(&r, RestorePhase::Completed));
        // ...and the domain condition still survives the heal.
        let conds = &r.status.as_ref().unwrap().conditions;
        assert!(
            conds
                .iter()
                .any(|c| c.type_ == "Resolved" && c.status == "True")
        );
    }

    #[test]
    fn ready_status_in_flight_is_reconciling() {
        let r = restore_with_condition("Resolved", "True");
        let v = restore_ready_status(
            &r,
            RestorePhase::Restoring,
            "MoverJobRunning",
            "the restore mover Job is in flight",
        );
        assert_eq!(v["phase"], "Restoring");
        assert_eq!(cond(&v, "Ready")["status"], "False");
        assert_eq!(cond(&v, "Reconciling")["status"], "True");
        assert_eq!(cond(&v, "Reconciling")["reason"], "MoverJobRunning");
        assert_eq!(cond(&v, "Stalled")["status"], "False");
    }

    fn pvc(value: serde_json::Value) -> k8s_openapi::api::core::v1::PersistentVolumeClaim {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn pvc_claims_restore_matches_only_our_datasourceref() {
        let claim = pvc(serde_json::json!({
            "metadata": { "name": "qui", "namespace": "downloads" },
            "spec": { "dataSourceRef": {
                "apiGroup": "kopiur.home-operations.com", "kind": "Restore", "name": "qui",
            } },
        }));
        assert!(pvc_claims_restore(&claim, "qui"));
        assert!(!pvc_claims_restore(&claim, "other"));

        // Wrong apiGroup (a VolSync ReplicationDestination) must not match.
        let volsync = pvc(serde_json::json!({
            "metadata": { "name": "qui", "namespace": "downloads" },
            "spec": { "dataSourceRef": {
                "apiGroup": "volsync.backube", "kind": "ReplicationDestination", "name": "qui",
            } },
        }));
        assert!(!pvc_claims_restore(&volsync, "qui"));

        // No dataSourceRef at all.
        let plain = pvc(serde_json::json!({ "metadata": { "name": "qui" }, "spec": {} }));
        assert!(!pvc_claims_restore(&plain, "qui"));
    }

    #[test]
    fn pvc_is_bound_reads_volume_name_or_phase() {
        assert!(pvc_is_bound(&pvc(serde_json::json!({
            "metadata": { "name": "p" }, "spec": { "volumeName": "pvc-123" },
        }))));
        assert!(pvc_is_bound(&pvc(serde_json::json!({
            "metadata": { "name": "p" }, "spec": {}, "status": { "phase": "Bound" },
        }))));
        assert!(!pvc_is_bound(&pvc(serde_json::json!({
            "metadata": { "name": "p" }, "spec": {}, "status": { "phase": "Pending" },
        }))));
    }
}
