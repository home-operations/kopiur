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

use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};

use kopiur_api::common::{PhaseLabel, RepositoryRef};
use kopiur_api::expand::{populate_job_name, restore_source_path};
use kopiur_api::restore::ResolvedRestore;
use kopiur_api::snapshot::Snapshot;
use kopiur_api::{
    OnMissingSnapshot, ResolutionOutcome, Restore, RestoreClaimPhase, RestorePhase, RestoreSource,
    RestoreTarget, validate,
};
use kopiur_mover::workspec::{
    MoverOptions, MoverWorkSpec, Operation, RepositoryConnect, ResolvedIdentity as MoverIdentity,
    RestoreOp, RestoreSelection, RestoreSelector, SnapshotAnchor, TargetRef,
};

use crate::config;
use crate::consts::{
    ALLOW_PRIVILEGED_MOVER_ACTION, API_VERSION, CREDENTIALS_AVAILABLE_CONDITION,
    CREDENTIALS_PROJECTED_REASON, INHERIT_FALLBACK_REASON, MATCH_WORKLOAD_SECURITY_CONTEXT_ACTION,
    MISSING_RECORDED_IDENTITY_REASON, MOVER_JOB_FAILED_REASON, MOVER_PERMITTED_CONDITION,
    MOVER_POD_WEDGED_REASON, NO_SNAPSHOT_CONTINUE_REASON, ORPHANED_PRIME_REAPED_REASON,
    POPULATE_HIJACKED_REASON, PRIVILEGED_MOVER_NOT_PERMITTED_REASON, RECORDED_APPLIED_REASON,
    RECORDED_PINNED_NO_UID_REASON, RECREATE_CLAIM_TO_RESTORE_ACTION,
    RESTORE_SECURITY_CONTEXT_COMPATIBLE_CONDITION, RESTORE_SNAPSHOT_NOT_FOUND_REASON,
    RESTORE_SOURCE_RESOLVED_REASON, SECURITY_CONTEXT_COMPATIBLE_REASON,
    SECURITY_CONTEXT_INHERITED_CONDITION, SET_EXPLICIT_MOVER_CONTEXT_ACTION,
    SOURCE_PATH_AMBIGUOUS_REASON,
};
use crate::context::Context;
use crate::error::{Error, Result, error_policy_for};
use crate::io::{self, ResolvedRepository};
use crate::jobs::{self, JobLimits, MoverJobInputs, VolumeMountSpec};

mod plan;

pub use plan::*;

#[cfg(test)]
mod tests;
/// The source-resolution decision the reconcile loop acts on: a concrete snapshot
/// id (controller-resolved/pinned), a deliberate "no snapshot, come up empty"
/// (deploy-or-restore), or a selector the MOVER resolves in-Job (object stores,
/// where the controller can't list the backend in-process).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The source resolved to (or was pinned to) this kopia snapshot id.
    Snapshot(String),
    /// `onMissingSnapshot: Continue` with no matching snapshot — provision an empty volume.
    Empty,
    /// `fromPolicy`/`identity`-without-id on a backend the controller can't list
    /// in-process: dispatch the restore Job with this selector and let the mover
    /// resolve "latest"/offset/asOf (and pin `status.resolved` itself).
    Deferred(RestoreSelector),
}

/// The already-pinned resolution decision for a DIRECT-target restore, or `None`
/// when the source still has to be resolved. Pure + exhaustive, so the
/// data-safety invariant (ADR §4.6: "pinned once, never re-resolved") lives in
/// one testable place. Cases:
/// - `resolution: NoSnapshot` pinned → [`Resolution::Empty`] (a later snapshot never retargets).
/// - `kopiaSnapshotID` pinned (with or without the newer `resolution: Snapshot`, so a
///   legacy pin written before that field existed still reads correctly) → [`Resolution::Snapshot`].
/// - Otherwise → `None` (resolve now).
///
/// It takes NO phase. A direct restore is TERMINAL at the reconcile guard once
/// `Completed`/`Failed` ([`phase_is_terminal_at_guard`]), so it can never reach
/// resolution in a settled phase, and there is nothing here to back-fill from.
/// The populator's
/// three-way `Completed` reading — including the pre-fix back-fill and the #233
/// already-bound carve-out — is per claim now, in
/// [`plan::claim_pinned_decision`], keyed on the claim's own pin and phase
/// rather than on the whole `Restore`'s (#443).
fn pinned_decision(resolved: Option<&ResolvedRestore>) -> Option<Resolution> {
    match resolved {
        Some(r) if r.resolution == Some(ResolutionOutcome::NoSnapshot) => Some(Resolution::Empty),
        Some(r) => r.kopia_snapshot_id.clone().map(Resolution::Snapshot),
        None => None,
    }
}

/// Reconcile a `Restore`.
#[tracing::instrument(skip(restore, ctx), fields(kind = "Restore", namespace = %restore.namespace().unwrap_or_default(), name = %restore.name_any()))]
pub async fn reconcile(restore: Arc<Restore>, ctx: Arc<Context>) -> Result<Action> {
    let start = std::time::Instant::now();
    let result = reconcile_inner(&restore, &ctx).await;
    ctx.metrics
        .record_reconcile("Restore", start.elapsed().as_secs_f64());
    // The Restore lifecycle phase is a store-backed observable gauge
    // (`kopiur_resource_phase`, see
    // [`crate::metrics::Metrics::register_resource_observers`]); nothing to mirror
    // here.
    //
    // Duration is a different story, and was the one gauge in the whole crate
    // that could never survive a restart: it is an imperative gauge written once
    // at the Job-completion site, and a finished Restore never re-runs its mover,
    // so the series vanished for good on every process restart. Re-derive it from
    // the pinned status timing instead.
    reseed_restore_duration(&restore, &ctx);
    result
}

/// Re-derive `kopiur_restore_duration_seconds` from `status.timing` so the series
/// survives a controller restart.
///
/// Mirrors how the policy reconciler re-seeds
/// `kopiur_snapshot_verified_timestamp_seconds` from `status.lastVerified`: the
/// CR is the durable record, the gauge is just a projection of it. Silent on
/// anything missing or malformed — an absent series is honest, a wrong one is not.
fn reseed_restore_duration(restore: &Restore, ctx: &Context) {
    let Some(namespace) = restore.namespace() else {
        return;
    };
    let Some(timing) = restore.status.as_ref().and_then(|s| s.timing.as_ref()) else {
        return;
    };
    let (Some(start), Some(end)) = (timing.start_time.as_deref(), timing.end_time.as_deref())
    else {
        return;
    };
    let (Some(start), Some(end)) = (
        crate::snapshot_policy::rfc3339_unix_secs(start),
        crate::snapshot_policy::rfc3339_unix_secs(end),
    ) else {
        return;
    };
    // A negative duration means clock skew between the mover pod and whatever
    // wrote startTime; publishing it would be worse than publishing nothing.
    if let Some(secs) = end.checked_sub(start).filter(|s| *s >= 0) {
        ctx.metrics
            .set_restore_duration(&namespace, &restore.name_any(), secs);
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

    // A populator restore rebinds cluster-scoped PersistentVolumes and reads
    // StorageClasses — permanent 403s under a namespaced install's Role RBAC.
    // Refuse up front with the fix (Structural: surfaced as an event + long
    // requeue) instead of letting the handshake wedge on retried 403s while
    // the consumer PVC sits Pending with no explanation.
    if matches!(restore.spec.target, RestoreTarget::Populator(_))
        && matches!(ctx.watch_scope, crate::config::WatchScope::Namespaced(_))
    {
        return Err(Error::Validation(populator_needs_cluster_scope_message()));
    }

    let state = populator_state(&restore.spec.target);

    // Already terminal: a Restore is one-shot. Once Completed/Failed there is
    // nothing left to do until the spec changes, so don't re-resolve, re-pin a
    // fresh timestamp, or re-write the phase — each of which would churn status and
    // self-trigger another reconcile (the same hot-loop class as the repo bug).
    // Mirrors the Snapshot reconciler's terminal discipline. (A `Completed` populator
    // is NOT terminal here — see `phase_is_terminal_at_guard`.)
    match restore.status.as_ref().and_then(|s| s.phase.as_ref()) {
        Some(phase) if phase_is_terminal_at_guard(phase, state) => {
            return steady_terminal_restore(restore, &api, &name, phase).await;
        }
        // A phase this build cannot read is NOT terminal, so the reconcile
        // proceeds and will re-derive (and overwrite) the phase from the mover
        // Job below. That self-heal is the right default — the resolution is
        // PINNED in `status.resolved` and never re-resolved, so re-driving
        // restores the same snapshot into the same target rather than
        // retargeting — but it must never be silent.
        Some(phase @ RestorePhase::Unknown(_)) => {
            io::warn_unreadable_phase("Restore", &namespace, &name, phase.label());
        }
        Some(
            RestorePhase::Pending
            | RestorePhase::Resolving
            | RestorePhase::Restoring
            | RestorePhase::Completed
            | RestorePhase::Failed,
        )
        | None => {}
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

    // Repository-readiness gate (the Restore peer of the Snapshot reconciler's,
    // #345): don't act on a restore whose repository is not `Ready` (backend
    // unreachable — the breaker is open). It runs HERE — after the terminal guard
    // (so terminal restores are untouched) and BEFORE the resolution/`waitTimeout`
    // machinery and any mover dispatch — because everything past this point either
    // applies `onMissingSnapshot` once the wait window closes (for `fromPolicy`
    // that defaults to `Continue`, which would provision an EMPTY volume off the
    // back of an outage) or fans out a mover Job that can only fail
    // `kopia repository connect` — and a Failed Restore is terminal (one-shot),
    // so a transient outage would permanently fail it. A not-Ready repository
    // therefore DEFERS (requeue); it never fakes a missing-snapshot outcome and
    // never launches a doomed Job.
    //
    // Matched exhaustively (CLAUDE.md), and both parking arms return BEFORE
    // `ensure_wait_anchor` below — which is the whole of #393: a restore whose
    // repository could not even be looked up must not open (and start spending)
    // its `waitTimeout` window.
    //
    // The gate hands back THE `Restore` the rest of this pass must use, and
    // `restore` is rebound to it: unchanged in the ordinary case, and a carried
    // copy holding the cleared conditions when the gate cleared a stale
    // `ReferentAvailable=False`. Every condition writer below rebuilds the array
    // from this `restore`, and four of them (the gate parks in
    // `run_restore_mover`) patch UNCONDITIONALLY — continuing from the
    // reconcile-start copy would re-write the `False` just cleared and alternate
    // the two writes on every requeue, forever. See `restore_with_conditions`.
    let gated;
    let restore = match gate_on_repository_readiness(ctx, restore, &api, &namespace, &name).await? {
        RepositoryGate::Proceed(carried) => {
            gated = carried;
            gated.get()
        }
        RepositoryGate::Held(action) | RepositoryGate::Undetermined(action) => {
            return Ok(action);
        }
    };

    // Populator vs direct target: two entirely different pipelines, and #443 is
    // why the split is HERE rather than after a shared resolution block. A
    // populator resolves PER CLAIM — each claiming PVC gets its own kopia source
    // path, its own pin and its own `waitTimeout` window — so a single top-level
    // `status.resolved` written before the dispatch would pin whichever claim
    // resolved first and hand it to every sibling. Exhaustive over
    // [`PopulatorState`].
    match state {
        PopulatorState::AwaitingClaim => {
            // ONE namespaced PVC LIST for the pass: every claimant, plus the
            // prime PVCs this Restore owns (which is what lets a settled claim's
            // "is my prime really gone?" check cost no extra GET).
            let claimants = claiming_pvcs(ctx, &namespace, &name, restore.uid().as_deref()).await?;
            // #380: the Restore-level `waitTimeout` anchor still opens on the
            // first pass that gets past the readiness gate AND finds a claim, so
            // a standing GitOps populator does not burn its window sitting idle.
            // Each claim then measures its OWN window from that anchor (or from
            // its own later stamp) via `claim_wait_window`.
            ensure_wait_anchor(
                restore,
                &api,
                &namespace,
                &name,
                state,
                !claimants.consumers.is_empty(),
            )
            .await?;
            drive_populator_fanout(ctx, restore, &api, &namespace, &name, claimants).await
        }
        PopulatorState::DirectTarget => {
            drive_direct_target(ctx, restore, &api, &namespace, &name, state).await
        }
    }
}

/// Resolve and dispatch a restore with an EXPLICIT target (`target.pvc` /
/// `target.pvcRef`): open the `waitTimeout` window, resolve the source once, pin
/// it to the top-level `status.resolved`, and hand it to
/// [`drive_direct_restore`].
///
/// Split out of `reconcile_inner` by #443: this whole block used to run for a
/// populator too, writing a Restore-level `Resolving` + `status.resolved` before
/// the target dispatch. A fanned-out populator has no single resolution to pin,
/// so it bypasses this entirely and resolves inside [`resolve_claim_source`],
/// once per claim.
async fn drive_direct_target(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    state: PopulatorState,
) -> Result<Action> {
    // The PVC this restore fills — and, since #443, the PVC its per-PVC kopia
    // source path is derived from. A direct restore against a `pvcSelector`
    // policy carried the SAME cross-volume hazard as the populator: a pathless
    // identity matches the newest snapshot of ANY member. Exhaustive over
    // `RestoreTarget`.
    let source_target = match &restore.spec.target {
        RestoreTarget::Pvc(t) => kopiur_api::snapshot::PvcTargetRef {
            namespace: namespace.to_string(),
            name: t.name.clone(),
        },
        RestoreTarget::PvcRef(r) => kopiur_api::snapshot::PvcTargetRef {
            namespace: r.namespace.clone().unwrap_or_else(|| namespace.to_string()),
            name: r.name.clone(),
        },
        RestoreTarget::Populator(_) => {
            return Err(Error::Invariant(
                "DirectTarget restore reached with a populator target (should route to \
                 AwaitingClaim)"
                    .into(),
            ));
        }
    };
    let mut cache = PassCache::default();

    // #380: the `waitTimeout` window opens HERE — on the first pass that gets past the
    // readiness gate — not at the Restore's creation. The returned window carries the
    // anchor in effect for THIS pass, and is what the wait below is measured from: the
    // freshly stamped value rather than a re-read of `restore`, because the pass that
    // OPENS the window is exactly the pass that must not measure it from creation.
    let wait_window = ensure_wait_anchor(restore, api, namespace, name, state, true).await?;

    let on_missing = effective_on_missing(
        restore
            .spec
            .policy
            .as_ref()
            .and_then(|p| p.on_missing_snapshot),
        &restore.spec.source,
    );

    // ADR §4.6: the resolution is pinned ONCE and never re-resolved — a restore must
    // not silently retarget when newer snapshots appear mid-flight. The pinned decision
    // is a snapshot id OR a deliberate "no snapshot, deploy-or-restore" (`Resolution`);
    // `pinned_decision` reads it (incl. legacy pins written before the
    // `resolution` field existed), returning `None` only when the source still
    // has to be resolved.
    let resolved = restore.status.as_ref().and_then(|s| s.resolved.as_ref());

    let decision = match pinned_decision(resolved) {
        Some(d) => d,
        None => {
            // The per-PVC derivation can refuse (a selector policy that names no
            // single volume for this target). Fail CLOSED and terminally rather
            // than restoring under a pathless identity — see #443.
            let outcome = match resolve_snapshot(
                ctx,
                restore,
                namespace,
                wait_window.anchor(),
                &source_target,
                &mut cache,
            )
            .await?
            {
                SourceResolution::Resolved(outcome) => outcome,
                SourceResolution::Ambiguous(msg) => {
                    return stall_source_path_ambiguous(restore, api, name, &msg).await;
                }
            };
            match outcome {
                Some(ResolveOutcome::Pinned(res)) => {
                    // Pin the FULL resolution (outcome + id + provenance + timestamp) exactly
                    // once; the no-pin check above makes this a single write, so it cannot
                    // churn status.
                    let mut resolved = serde_json::json!({
                        "kopiaSnapshotID": res.kopia_snapshot_id,
                        "pinnedAt": chrono::Utc::now().to_rfc3339(),
                    });
                    resolved["resolution"] = serde_json::to_value(ResolutionOutcome::Snapshot)?;
                    if let Some(r) = &res.snapshot_ref {
                        resolved["snapshotRef"] = serde_json::to_value(r)?;
                    }
                    if let Some(i) = &res.identity {
                        resolved["identity"] = serde_json::to_value(i)?;
                    }
                    let mut status = restore_ready_status(
                        restore,
                        RestorePhase::Resolving,
                        RESTORE_SOURCE_RESOLVED_REASON,
                        "the restore source resolved to a concrete kopia snapshot \
                         (pinned to status.resolved)",
                    );
                    status["resolved"] = resolved;
                    io::patch_status(api, name, status).await?;
                    Resolution::Snapshot(res.kopia_snapshot_id)
                }
                // Object stores can't be listed in-process, so the controller resolves
                // only the identity and defers snapshot selection to the mover Job. Do
                // NOT pin here — the mover pins `status.resolved` once it resolves
                // "latest"/offset/asOf (or NoSnapshot under Continue). The driver's
                // job-exists guard makes re-dispatch across requeues idempotent while
                // unpinned, so the pin-once invariant holds (a one-shot Restore is
                // terminal after the Job, and never re-resolves).
                Some(ResolveOutcome::Deferred(sel)) => Resolution::Deferred(sel),
                None => {
                    // No snapshot matched. While the `waitTimeout` window (opened when this
                    // restore could first proceed — `ensure_wait_anchor`) is open, keep
                    // waiting instead of giving up — `onMissingSnapshot` applies only once
                    // the window closes (ADR §4.6 G7).
                    let now = chrono::Utc::now().timestamp();
                    let wait_timeout = restore
                        .spec
                        .policy
                        .as_ref()
                        .and_then(|p| p.wait_timeout.as_deref());
                    if let Some(remaining) =
                        wait_remaining_secs(wait_window.anchor(), wait_timeout, now)
                    {
                        // Report the REAL blocker, at the cadence that state deserves
                        // (`wait_park_report`, pure + exhaustive). Static messages (no
                        // countdown): an identical re-patch is a server-side no-op, so
                        // polling here cannot churn status.
                        let (reason, msg, requeue) =
                            wait_park_report(wait_window, wait_timeout, remaining);
                        let conditions = io::upsert_condition(
                            &existing_conditions(restore),
                            "Resolved",
                            false,
                            reason,
                            &msg,
                            restore.metadata.generation,
                        );
                        io::patch_status(
                            api,
                            name,
                            restore_ready_status_on(
                                restore,
                                &conditions,
                                RestorePhase::Pending,
                                reason,
                                &msg,
                            ),
                        )
                        .await?;
                        return Ok(Action::requeue(std::time::Duration::from_secs(requeue)));
                    }
                    // Window closed (or none configured): honor the closed enum exhaustively.
                    match on_missing {
                        OnMissingSnapshot::Fail => {
                            let msg = "no snapshot matched the restore source within the \
                                       waitTimeout window; fix spec.source (or create the \
                                       missing snapshot) and create a NEW Restore — a Failed \
                                       Restore is terminal and never retries";
                            let conditions = io::upsert_condition(
                                &existing_conditions(restore),
                                "Resolved",
                                false,
                                RESTORE_SNAPSHOT_NOT_FOUND_REASON,
                                msg,
                                restore.metadata.generation,
                            );
                            io::patch_status(
                                api,
                                name,
                                restore_ready_status_on(
                                    restore,
                                    &conditions,
                                    RestorePhase::Failed,
                                    RESTORE_SNAPSHOT_NOT_FOUND_REASON,
                                    msg,
                                ),
                            )
                            .await?;
                            return Err(Error::MissingDependency(
                                "no snapshot matched restore source".into(),
                            ));
                        }
                        // Deploy-or-restore: no snapshot, so the volume comes up empty. Do NOT
                        // complete-and-return here — fall through to the target driver, which
                        // provisions the empty volume (an empty prime PVC for a populator; an
                        // empty `target.pvc` for a direct restore). The decision is pinned
                        // below so a later-appearing snapshot never retargets it (ADR §4.6).
                        OnMissingSnapshot::Continue => Resolution::Empty,
                    }
                }
            }
        }
    };

    // Dispatch the decision to the target driver. Exhaustive over `Resolution`, so a new
    // variant must be handled before it compiles. The mover work is a concrete id, an
    // in-Job selector, or nothing (the deploy-or-restore empty volume — the only case
    // that skips the mover).
    let selection = match decision {
        Resolution::Snapshot(id) => Some(RestoreSelection::Snapshot(id)),
        Resolution::Deferred(sel) => Some(RestoreSelection::Resolve(sel)),
        Resolution::Empty => None,
    };
    // A direct restore always acts on its decision, so pin an empty outcome here.
    if selection.is_none() {
        pin_no_snapshot(api, restore, name).await?;
    }
    drive_direct_restore(
        ctx,
        restore,
        api,
        namespace,
        name,
        selection.as_ref(),
        &source_target,
    )
    .await
}

/// Park a restore whose per-PVC kopia source path could not be derived (#443).
///
/// A SPEC problem, not a missing object: the `Restore` and the `SnapshotPolicy`
/// are each valid, but together they name no single volume for this target, and
/// restoring under a pathless identity would take the newest snapshot of any
/// member — i.e. quite possibly another volume's data. So it fails closed,
/// terminally (`Failed`/`Stalled`), and the message names the field that fixes
/// it.
async fn stall_source_path_ambiguous(
    restore: &Restore,
    api: &Api<Restore>,
    name: &str,
    msg: &str,
) -> Result<Action> {
    let conditions = io::upsert_condition(
        &existing_conditions(restore),
        "Resolved",
        false,
        SOURCE_PATH_AMBIGUOUS_REASON,
        msg,
        restore.metadata.generation,
    );
    io::patch_status(
        api,
        name,
        restore_ready_status_on(
            restore,
            &conditions,
            RestorePhase::Failed,
            SOURCE_PATH_AMBIGUOUS_REASON,
            msg,
        ),
    )
    .await?;
    tracing::warn!(restore = %name, "{msg}");
    // Terminal for THIS restore: only a spec edit clears it, so heartbeat on the
    // slow structural cadence rather than returning an error the policy would
    // retry (and count) every 30 s.
    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

/// Durably pin the deploy-or-restore "no snapshot" decision, so a snapshot that appears
/// LATER can never silently restore over the volume we are about to provision empty
/// (ADR §4.6). Idempotent: an already-`NoSnapshot` resolution is left alone.
///
/// Pinned at the point of USE, not at resolution. A populator that turns out to have
/// nothing to populate (its claiming PVC is already bound — #233) must never record "this
/// restore decided to come up empty", because that pin outlives the no-op: the user then
/// follows the documented fix, re-creates the claim to get a real restore, and
/// `pinned_decision` hands back `Empty` from the pin — provisioning an EMPTY volume and
/// silently never restoring their data. Only a pass that actually provisions may pin.
async fn pin_no_snapshot(api: &Api<Restore>, restore: &Restore, name: &str) -> Result<()> {
    if restore
        .status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .and_then(|r| r.resolution)
        == Some(ResolutionOutcome::NoSnapshot)
    {
        return Ok(());
    }
    let mut pin = serde_json::json!({ "pinnedAt": chrono::Utc::now().to_rfc3339() });
    pin["resolution"] = serde_json::to_value(ResolutionOutcome::NoSnapshot)?;
    io::patch_status(api, name, serde_json::json!({ "resolved": pin })).await
}

/// Park a populator `Restore` in `AwaitingClaim=True` / `Pending` with `reason`+`msg`
/// (no claiming PVC yet, or a WaitForFirstConsumer claim that hasn't been scheduled).
/// Report that this restore's `inheritSecurityContextFrom` could not resolve a workload pod and
/// its explicit `mover.securityContext` stood in. Warn-only — the run proceeds.
///
/// The restore peer of the backup's `report_inherit_outcome`. It carries only the `Fallback`
/// arm by design: `InheritPinnedNoUid` is backup-only (an fsGroup-only inherit is a blessed
/// restore shape), and `InheritOverridden` keys on reading the source, which a restore does not.
async fn report_restore_inherit_fallback(
    namespace: &str,
    restore: &Restore,
    reason: &str,
    ctx: &Context,
) {
    let message = format!(
        "{reason}. Proceeding with the recipe's explicit mover.securityContext, which pins the \
         mover's identity itself — so the restored files will be owned as that context says, not \
         as the workload named by inheritSecurityContextFrom."
    );
    // Not the first conditions writer in this reconcile — the privileged-mover gate and the
    // "clear stale MoverPermitted" block run above. Building `existing` from the
    // reconcile-start copy would let them erase this condition, which would then be re-written
    // and re-Evented on every subsequent reconcile. See `io::live_conditions_source`.
    let api: Api<Restore> = Api::namespaced(ctx.client.clone(), namespace);
    let name = restore.name_any();
    let Some(live) = io::live_conditions_source(&api, &name, restore).await else {
        return; // deleted mid-reconcile
    };
    let conditions = io::upsert_condition(
        &existing_conditions(&live),
        SECURITY_CONTEXT_INHERITED_CONDITION,
        false,
        INHERIT_FALLBACK_REASON,
        &message,
        restore.metadata.generation,
    );
    // Guard the Event behind a real transition: `publish_warning_event` has no dedup and a
    // Restore re-reconciles, so an unguarded write would re-fire the warning every pass.
    let current = serde_json::to_value(&live.status).ok();
    match io::patch_status_if_changed(
        &api,
        &name,
        current.as_ref(),
        serde_json::json!({ "conditions": conditions }),
    )
    .await
    {
        Ok(true) => {
            io::publish_warning_event(
                ctx,
                restore,
                INHERIT_FALLBACK_REASON,
                MATCH_WORKLOAD_SECURITY_CONTEXT_ACTION,
                &message,
            )
            .await
        }
        Ok(false) => {}
        Err(e) => tracing::debug!(error = %e, "restore inherit fallback: condition patch failed"),
    }
}

/// Park the restore on the `MissingRecordedIdentity` hold:
/// `SecurityContextInherited=False` + `Pending` + a Warning Event (mirrors the
/// missing-ServiceAccount pattern above). The hold is permanent-*shaped* — a
/// pre-feature/foreign snapshot, or a catalog scan that has not materialized the
/// matching CR yet — so the caller requeues it on the slow structural cadence via
/// [`Error::MissingRecordedIdentity`], never the fast transient one.
async fn report_missing_recorded_identity(
    restore: &Restore,
    api: &Api<Restore>,
    msg: &str,
    ctx: &Context,
) -> Result<()> {
    let name = restore.name_any();
    // Not necessarily the first conditions writer across reconciles — build from the
    // LIVE conditions and only patch (and Event) on a real transition, so the 300s
    // requeue does not re-fire an identical Warning forever.
    let Some(live) = io::live_conditions_source(api, &name, restore).await else {
        return Ok(()); // deleted mid-reconcile
    };
    let conditions = io::upsert_condition(
        &existing_conditions(&live),
        SECURITY_CONTEXT_INHERITED_CONDITION,
        false,
        MISSING_RECORDED_IDENTITY_REASON,
        msg,
        restore.metadata.generation,
    );
    let current = serde_json::to_value(&live.status).ok();
    if io::patch_status_if_changed(
        api,
        &name,
        current.as_ref(),
        serde_json::json!({ "phase": "Pending", "conditions": conditions }),
    )
    .await?
    {
        io::publish_warning_event(
            ctx,
            restore,
            MISSING_RECORDED_IDENTITY_REASON,
            SET_EXPLICIT_MOVER_CONTEXT_ACTION,
            msg,
        )
        .await;
    }
    Ok(())
}

/// The `SecurityContextInherited` verdict for a restore that inherited the identity
/// RECORDED on its backup (`inheritSecurityContextFrom.snapshot`).
struct RecordedInheritVerdict {
    /// `true` when the record contributed a real identity to reproduce.
    ok: bool,
    reason: &'static str,
    message: String,
}

/// Decide what inheriting a backup's recorded identity actually achieved. Pure — every
/// arm is unit-testable without a cluster.
///
/// Reuses the pins-identity-vs-baseline logic from live-pod inherit: a record that
/// contributes nothing beyond the mover's own hardened baseline (no uid, no gid, and an
/// fsGroup that is absent or the hardened 65532) is a provable no-op — the mover runs
/// as its image's uid — and must warn (`RecordedPinnedNoUid`), not claim success. A
/// non-baseline fsGroup-only record is the blessed restore shape (the kubelet applies
/// fsGroup to the fresh target volume) and reports `True`, as does a pinned uid.
///
/// The message ALWAYS names the Snapshot, the recorded uid, and the provenance —
/// only `src: inherited` may claim the identity tracked the workload — and a recorded
/// ROOT uid is called out explicitly: recorded metadata is untrusted repository data
/// (anyone with repository write credentials can forge `{"schema":1,"uid":0}`), so the
/// elevation must stay auditable in the condition text even in namespaces already
/// annotated for privileged movers (plan §3.4).
fn recorded_inherit_verdict(
    snapshot: &str,
    uid: Option<i64>,
    src: kopiur_api::recorded::RecordedSrc,
    gid: Option<i64>,
    fs_group: Option<i64>,
) -> RecordedInheritVerdict {
    use kopiur_api::common::MOVER_NONROOT_ID;
    use kopiur_api::recorded::RecordedSrc;

    // Exhaustive over the provenance so a new variant must state its honesty here.
    let provenance = match src {
        RecordedSrc::Inherited => {
            "recorded provenance `inherited`: the identity was read from the live workload \
             at backup time, so this restore reproduces the identity the workload actually \
             ran as"
        }
        RecordedSrc::Explicit => {
            "recorded provenance `explicit`: the backup recipe's explicit mover context \
             pinned this identity — it reproduces the backup mover's identity, which was \
             never workload-derived"
        }
        RecordedSrc::Defaults => {
            "recorded provenance `defaults`: the identity came from repository/hardened \
             mover defaults at backup time, not from the workload"
        }
        RecordedSrc::Unknown => {
            "recorded provenance is a value this operator version does not recognize \
             (written by a newer kopiur) — whether it tracked the workload is unknown"
        }
    };

    let contributes =
        uid.is_some() || gid.is_some() || fs_group.is_some_and(|f| f != MOVER_NONROOT_ID);
    if !contributes {
        return RecordedInheritVerdict {
            ok: false,
            reason: RECORDED_PINNED_NO_UID_REASON,
            message: format!(
                "Snapshot `{snapshot}` recorded no pinned uid (and no gid/fsGroup beyond the \
                 mover's defaults), so inheriting it contributed nothing: the mover runs as its \
                 image's uid {MOVER_NONROOT_ID}, which did NOT come from the backup \
                 ({provenance}). Fix: pin mover.securityContext.runAsUser on this Restore if the \
                 restored files must be owned by a specific uid."
            ),
        };
    }

    let identity = match uid {
        Some(u) => format!("uid {u}"),
        None => format!(
            "its image's uid {MOVER_NONROOT_ID} (the record pins no uid), with the \
             recorded group identity applied"
        ),
    };
    let detail = [
        gid.map(|g| format!("gid {g}")),
        fs_group.map(|f| format!("fsGroup {f}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(" ({detail})")
    };
    // A recorded ROOT uid must be visible in the condition itself: the privileged-mover
    // gate + Events fire elsewhere, but this text is what makes a FORGED uid-0 tag
    // auditable per-restore, naming both the elevation and where it came from.
    let root_note = if uid == Some(0) {
        format!(
            " The mover runs as ROOT (uid 0): Snapshot `{snapshot}` recorded uid 0, and \
             recorded metadata is repository data forgeable by anyone with repository write \
             access — verify this snapshot is trusted; the run is also gated on the namespace's \
             privileged-movers opt-in."
        )
    } else {
        String::new()
    };
    RecordedInheritVerdict {
        ok: true,
        reason: RECORDED_APPLIED_REASON,
        message: format!(
            "the mover inherited the identity recorded on Snapshot `{snapshot}` and runs \
             as {identity}{detail}; {provenance}.{root_note}"
        ),
    }
}

/// Write the recorded-inherit verdict as the `SecurityContextInherited` condition
/// (True or False) and, on a warning transition, a Warning Event. Mirrors
/// [`report_restore_inherit_fallback`]'s two-writer discipline: live conditions +
/// patch-if-changed, so the privileged-gate/stale-clear writers above are not erased
/// and a steady state re-fires nothing.
async fn report_restore_recorded_inherit(
    namespace: &str,
    restore: &Restore,
    verdict: &RecordedInheritVerdict,
    ctx: &Context,
) {
    let api: Api<Restore> = Api::namespaced(ctx.client.clone(), namespace);
    let name = restore.name_any();
    let Some(live) = io::live_conditions_source(&api, &name, restore).await else {
        return; // deleted mid-reconcile
    };
    let conditions = io::upsert_condition(
        &existing_conditions(&live),
        SECURITY_CONTEXT_INHERITED_CONDITION,
        verdict.ok,
        verdict.reason,
        &verdict.message,
        restore.metadata.generation,
    );
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
                restore,
                verdict.reason,
                SET_EXPLICIT_MOVER_CONTEXT_ACTION,
                &verdict.message,
            )
            .await;
        }
        Ok(_) => {}
        Err(e) => {
            tracing::debug!(error = %e, "restore recorded inherit: condition patch failed");
        }
    }
}

// --- #443: the fanned-out populator driver ---------------------------------
//
// A populator `Restore` is claimed by EVERY PVC whose `spec.dataSourceRef` names
// it, not just the first one a LIST happened to return. Each claimant gets its
// own prime PVC, mover `Job`, per-PVC kopia source path and record under
// `status.claims.<pvc>`; the Restore's own phase is the AGGREGATE
// (`plan::aggregate_claims`). One claim failing stalls the Restore but never
// stops its siblings, and re-creating that claiming PVC re-arms it.

/// Everything ONE namespaced `PersistentVolumeClaim` LIST tells the populator
/// path — the claimants to drive, and the prime PVCs this `Restore` already
/// owns.
///
/// Both come from the same LIST on purpose: the unfiltered namespaced LIST is
/// the populator path's per-pass cost, and the "is this settled claim's prime
/// really gone?" check (which gates the cheap 600 s heartbeat) would otherwise
/// be one GET per claim on top of it.
struct Claimants {
    /// Every PVC whose `dataSourceRef` claims this `Restore`, in NAME order —
    /// the order claims are driven in, so a `maxConcurrentJobs`-capped
    /// repository admits them deterministically.
    consumers: Vec<k8s_openapi::api::core::v1::PersistentVolumeClaim>,
    /// Names of the LIVE (not terminating) prime PVCs this `Restore` owns.
    primes: std::collections::BTreeSet<String>,
}

impl Claimants {
    /// Whether `prime-<uid>` is still standing for this claim.
    fn prime_live(&self, uid: &str) -> bool {
        self.primes.contains(&prime_pvc_name(uid))
    }
}

/// LIST the namespace once and split it into the PVCs claiming this `Restore`
/// ([`pvc_claims_restore`]) and the prime PVCs it owns (labelled
/// `op=restore-populate` AND owned by this very `Restore`, so a sibling
/// populator's primes in the same namespace are never mistaken for ours).
///
/// Replaces the pre-#443 `claiming_pvc`, which `.find()`-ed the FIRST claimant
/// and left claimants 2..N `Pending` forever while the `Restore` reported
/// `Completed`.
async fn claiming_pvcs(
    ctx: &Context,
    namespace: &str,
    name: &str,
    restore_uid: Option<&str>,
) -> Result<Claimants> {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
    let items = pvc_api.list(&kube::api::ListParams::default()).await?.items;
    let mut consumers = Vec::new();
    let mut primes = std::collections::BTreeSet::new();
    for pvc in items {
        if pvc_claims_restore(&pvc, name) {
            consumers.push(pvc);
            continue;
        }
        let ours = pvc.metadata.deletion_timestamp.is_none()
            && pvc.metadata.labels.as_ref().is_some_and(|l| {
                l.get(crate::consts::OP_LABEL).map(String::as_str)
                    == Some(crate::consts::OP_RESTORE_POPULATE)
            })
            && pvc.metadata.owner_references.as_ref().is_some_and(|refs| {
                refs.iter()
                    .any(|r| r.kind == "Restore" && Some(r.uid.as_str()) == restore_uid)
            });
        if ours {
            primes.insert(pvc.name_any());
        }
    }
    consumers.sort_by_key(ResourceExt::name_any);
    Ok(Claimants { consumers, primes })
}

/// Every `PersistentVolume` our populator earmarked, indexed by the uid of the
/// claim its `claimRef` targets — [`our_rebound_pv`]'s per-claim predicate,
/// evaluated ONCE per pass over ONE cluster-wide LIST.
///
/// The uid index is what keeps the fan-out affordable: the pre-#443 code ran a
/// full, unfiltered `PersistentVolume` LIST per claim, so N claimants meant N
/// cluster-wide LISTs on every 5 s handshake requeue.
///
/// The uid match is load-bearing for the same reason it is in the single-claim
/// path: `claimRef` pins a specific PVC INSTANCE, so a PV left annotated from a
/// previous claim of the same name must not read as an outstanding rebind of
/// the current one.
async fn our_rebound_pvs(
    ctx: &Context,
    namespace: &str,
) -> Result<std::collections::BTreeMap<String, String>> {
    use k8s_openapi::api::core::v1::PersistentVolume;
    let pv_api: Api<PersistentVolume> = Api::all(ctx.client.clone());
    let mut by_uid = std::collections::BTreeMap::new();
    for pv in pv_api.list(&kube::api::ListParams::default()).await?.items {
        let annotated = pv
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|a| a.contains_key(PRIME_ORIGINAL_RECLAIM_ANNOTATION));
        if !annotated {
            continue;
        }
        let uid = pv
            .spec
            .as_ref()
            .and_then(|s| s.claim_ref.as_ref())
            .filter(|cr| cr.namespace.as_deref() == Some(namespace))
            .and_then(|cr| cr.uid.clone());
        if let Some(uid) = uid {
            by_uid.insert(uid, pv.name_any());
        }
    }
    Ok(by_uid)
}

/// The `fromPolicy` referents ONE fan-out pass resolves — the `SnapshotPolicy`
/// and the `identityDefaults` of the repository it belongs to.
///
/// Fetched once and shared by every claim: the per-PVC source path is derived
/// from the SAME policy for all of them, so N claims must not mean N
/// `SnapshotPolicy` GETs plus N repository resolutions on every 15 s requeue.
struct FromPolicyReferents {
    config: kopiur_api::SnapshotPolicy,
    namespace: String,
    identity_defaults: Option<kopiur_api::IdentityDefaults>,
}

/// Per-pass caches shared by every claim of one [`drive_populator_fanout`] pass.
#[derive(Default)]
struct PassCache {
    /// StorageClass name → whether it binds late (`WaitForFirstConsumer`). One
    /// cluster-scoped GET per distinct class, not per claim.
    wffc: std::collections::BTreeMap<String, bool>,
    /// The `fromPolicy` referents, resolved lazily on the first claim that
    /// actually needs to resolve a source.
    from_policy: Option<std::sync::Arc<FromPolicyReferents>>,
}

impl PassCache {
    /// Whether this claim's `StorageClass` binds late, memoized per class.
    async fn storage_class_is_wffc(
        &mut self,
        ctx: &Context,
        consumer: &k8s_openapi::api::core::v1::PersistentVolumeClaim,
    ) -> Result<bool> {
        let Some(scn) = consumer
            .spec
            .as_ref()
            .and_then(|s| s.storage_class_name.clone())
        else {
            return Ok(false);
        };
        if let Some(known) = self.wffc.get(&scn) {
            return Ok(*known);
        }
        let late = consumer_storage_class_is_wffc(ctx, consumer).await?;
        self.wffc.insert(scn, late);
        Ok(late)
    }

    /// The `fromPolicy` referents for this pass, fetched at most once.
    async fn policy_referents(
        &mut self,
        ctx: &Context,
        restore: &Restore,
        namespace: &str,
        from: &kopiur_api::restore::FromPolicy,
    ) -> Result<std::sync::Arc<FromPolicyReferents>> {
        if let Some(cached) = &self.from_policy {
            return Ok(cached.clone());
        }
        let cfg_ns = from.namespace.as_deref().unwrap_or(namespace).to_string();
        let cfg_api: Api<kopiur_api::SnapshotPolicy> = Api::namespaced(ctx.client.clone(), &cfg_ns);
        let config = cfg_api.get_opt(&from.name).await?.ok_or_else(|| {
            Error::MissingDependency(format!("SnapshotPolicy {cfg_ns}/{}", from.name))
        })?;
        let repo = resolve_restore_repository(ctx, restore, namespace).await?;
        let referents = std::sync::Arc::new(FromPolicyReferents {
            config,
            namespace: cfg_ns,
            identity_defaults: repo.identity_defaults.clone(),
        });
        self.from_policy = Some(referents.clone());
        Ok(referents)
    }
}

/// One claim's identity within a pass: the objects it owns and the record it is
/// building on.
struct ClaimContext<'a> {
    consumer: &'a k8s_openapi::api::core::v1::PersistentVolumeClaim,
    consumer_name: String,
    uid: String,
    prime_name: String,
    job_name: String,
    job_reuse: JobNameReuse,
    /// The record this claim carried into the pass (`None` for a fresh claim).
    prev: Option<&'a kopiur_api::RestoreClaimStatus>,
}

impl ClaimContext<'_> {
    /// A record for this claim in `phase`/`reason`, carrying forward every
    /// controller-owned field that is still true. Mover-owned fields
    /// (`observedAt`/`logTail`/`failure`) are deliberately left `None` — the
    /// controller never writes them, and [`claim_merge_body`] strips them so a
    /// controller pass can never blank what a claim's own mover wrote.
    fn record(
        &self,
        phase: RestoreClaimPhase,
        reason: ClaimReason,
        message: impl Into<String>,
    ) -> kopiur_api::RestoreClaimStatus {
        kopiur_api::RestoreClaimStatus {
            uid: Some(self.uid.clone()),
            phase: Some(phase),
            reason: Some(reason.as_str().to_string()),
            message: Some(message.into()),
            source_path: self.prev.and_then(|p| p.source_path.clone()),
            resolved: self.prev.and_then(|p| p.resolved.clone()),
            pvc_prime: self.prev.and_then(|p| p.pvc_prime.clone()),
            job: self.prev.and_then(|p| p.job.clone()),
            wait_started_at: self.prev.and_then(|p| p.wait_started_at.clone()),
            observed_at: None,
            log_tail: None,
            failure: None,
        }
    }
}

/// What one claim's pass produced: the record to merge, how soon to come back,
/// an Event to publish IF the claim actually transitioned, and any teardown that
/// must be SEQUENCED AFTER the status patch.
struct ClaimOutcome {
    record: kopiur_api::RestoreClaimStatus,
    requeue: u64,
    event: Option<ClaimEvent>,
    /// Deferred teardown for a completed handshake — see [`PostPatchFinalize`].
    after_patch: Option<PostPatchFinalize>,
}

/// A completed handshake's teardown, held back until the end-of-pass status
/// patch has landed.
///
/// It exists because ORDER is a data-safety property here: `finalize_populator`
/// strips the rebind annotation that identifies our PV, so it must never run
/// before the status write that records the restore as having happened. The
/// pre-#443 `finalize_populator_success` patched-then-finalized inline; the
/// fan-out has one status write per pass, so the finalize is carried out of the
/// per-claim driver and replayed after it.
struct PostPatchFinalize {
    job: String,
    prime: String,
    /// The PV whose stashed original reclaim policy to restore.
    pv: String,
}

/// A Warning Event a claim wants published, gated by [`claim_transition`] so the
/// 120 s heartbeat cannot inflate one Event's `count` forever.
struct ClaimEvent {
    reason: &'static str,
    action: &'static str,
    message: String,
}

/// Reap the populate artifacts of the claim recorded under `uid`: its prime PVC,
/// its mover `Job` + work-spec `ConfigMap`, and the `PersistentVolume` we
/// earmarked for it.
///
/// **Reason-gated, and that gate is a data-safety guard.**
/// [`fail_populate_hijacked`] leaves the prime PVC standing ON PURPOSE — it
/// holds the half-written restore, and a human may want it. Before the fan-out,
/// what protected it was the whole `Restore` short-circuiting at
/// [`phase_is_terminal_at_guard`] on `Failed`; the fan-out has to relax that
/// (one failed claim must not stop its siblings), so the protection lives HERE
/// instead — an exhaustive [`ClaimReason::artifacts_reapable`] match, never a
/// string compare that would silently stop matching if the literal moved.
///
/// **Idempotent, and silent when there is nothing to do.** It first LOOKS at
/// what actually exists and returns early — no deletes, no Event — when the
/// claim's artifacts are already gone. That matters twice: the ordinary "app
/// uninstalled" and "PVC re-applied over a `Populated` claim" flows would
/// otherwise publish a Warning saying a prime PVC and a mover Job were reaped
/// when both were finalized long ago; and the zero-claimant pass calls this for
/// every stale record it drops, which must not become a per-heartbeat write.
/// Every delete still tolerates an absent object, so it is safe on a
/// partially-completed handshake.
async fn reap_claim_artifacts(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
    claim_name: &str,
    uid: &str,
    reason: Option<&str>,
    pv: Option<&str>,
) -> Result<()> {
    if !claim_artifacts_reapable(reason) {
        // Either the hijacked case (the prime holds half-written data) or a
        // reason this build cannot read (fail closed, wave 2 finding 7). Both
        // leave `prime-<uid>` for a human.
        tracing::info!(
            %namespace, claim = %claim_name, reason = reason.unwrap_or("<none>"),
            prime = %prime_pvc_name(uid),
            "populator: keeping this claim's prime PVC — its recorded reason forbids automatic \
             reaping (a hijacked populate's prime holds half-written data; an absent or \
             unrecognized reason is never reaped on a guess); delete it by hand when done"
        );
        return Ok(());
    }
    let prime = prime_pvc_name(uid);
    let job = populate_job_name(&restore.name_any(), uid);
    // Legacy artifacts: a claim adopted from a pre-#443 status was driven under
    // the SHARED `{restore}-populate` name, so its Job/ConfigMap live there.
    let legacy_job = legacy_populate_job_name(&restore.name_any());

    let survivors = surviving_populate_artifacts(ctx, namespace, &prime, &job, &legacy_job).await?;
    // Nothing of ours is left and no PV of ours needs settling: say nothing.
    if survivors.described.is_empty() && pv.is_none() {
        return Ok(());
    }

    // A lost rebind's PV holds real restored data: force `Retain` so tearing the
    // claim down never reclaims it. With no PV of ours there is nothing to settle.
    let action = match pv {
        Some(pv) => PrimePvAction::ForceRetain(pv),
        None => PrimePvAction::Leave,
    };
    finalize_populator(ctx, namespace, &job, &prime, action).await?;
    if survivors.legacy_job {
        io::delete_mover_run(&ctx.client, namespace, &legacy_job).await?;
    }
    let note = reaped_populate_artifacts_note(&survivors.described, claim_name, pv);
    tracing::info!(%namespace, restore = %restore.name_any(), claim = %claim_name, "{note}");
    io::publish_warning_event(
        ctx,
        restore,
        ORPHANED_PRIME_REAPED_REASON,
        RECREATE_CLAIM_TO_RESTORE_ACTION,
        &note,
    )
    .await;
    Ok(())
}

/// Which of one claim's populate artifacts are still standing (and not already
/// terminating).
///
/// Split out of [`reap_claim_artifacts`] so the reaper stays one decision — "is
/// there anything to do, and may I do it?" — and so the *reporting* of what was
/// reaped names only what actually existed. An object already being deleted is
/// NOT an artifact to reap: re-issuing the delete (and re-publishing the Event)
/// on every pass while it sits on a finalizer would be pure churn.
struct SurvivingArtifacts {
    /// Human-readable descriptions, for the Event/log note. Empty ⇒ nothing left.
    described: Vec<String>,
    /// Whether the shared pre-#443 `{restore}-populate` Job is among them.
    legacy_job: bool,
}

async fn surviving_populate_artifacts(
    ctx: &Context,
    namespace: &str,
    prime: &str,
    job: &str,
    legacy_job: &str,
) -> Result<SurvivingArtifacts> {
    use k8s_openapi::api::batch::v1::Job;
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;

    let live = |meta: &kube::core::ObjectMeta| meta.deletion_timestamp.is_none();
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    let mut described = Vec::new();
    if pvc_api
        .get_opt(prime)
        .await?
        .is_some_and(|p| live(&p.metadata))
    {
        described.push(format!("prime PVC `{prime}`"));
    }
    if job_api
        .get_opt(job)
        .await?
        .is_some_and(|j| live(&j.metadata))
    {
        described.push(format!("populate Job `{job}`"));
    }
    let legacy = legacy_job != job
        && job_api
            .get_opt(legacy_job)
            .await?
            .is_some_and(|j| live(&j.metadata));
    if legacy {
        described.push(format!("legacy populate Job `{legacy_job}`"));
    }
    Ok(SurvivingArtifacts {
        described,
        legacy_job: legacy,
    })
}

/// The pre-#443 populate `Job` name — ONE name shared by every claim the
/// `Restore` ever populated. Only ever driven for a claim adopted from a legacy
/// status ([`adopt_legacy_claim`]); every fresh claim gets its own
/// [`populate_job_name`].
fn legacy_populate_job_name(restore: &str) -> String {
    format!("{restore}-populate")
}

/// CSI volume-populator handshake for `target.populator: {}` (ADR-0005 §9),
/// fanned out over EVERY claiming PVC (#443).
///
/// Each claimant is driven independently — its own prime PVC, mover `Job`,
/// per-PVC kopia source path and `status.claims.<pvc>` record — and the pass
/// ends in ONE status patch carrying the aggregate phase, the kstatus trio and
/// every claim merge. A claim that fails stalls the `Restore` (so `kubectl wait`
/// and Flux see the failure) without stopping its siblings; re-creating that
/// claiming PVC mints a new uid, which re-arms the claim.
async fn drive_populator_fanout(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    claimants: Claimants,
) -> Result<Action> {
    let prev: std::collections::BTreeMap<String, kopiur_api::RestoreClaimStatus> = restore
        .status
        .as_ref()
        .map(|s| s.claims.clone())
        .unwrap_or_default();

    // Zero claimants: the standing GitOps populator nothing has applied a
    // `dataSourceRef` for yet — or an app that has been torn down with its
    // populator left standing (the documented "living source" pattern).
    //
    // It is an END-OF-PASS writer like the N≥1 branch below, and that is
    // load-bearing: it must NULL the records of claimants that are gone in the
    // SAME patch that parks. Writing the park alone left them standing, so the
    // next pass 30 s later re-reaped them and re-published
    // `OrphanedPrimePvcReaped`, forever. With the nulls, the second pass is a
    // server-side no-op under the deep merge predicate.
    if claimants.consumers.is_empty() {
        return park_unclaimed_populator(ctx, restore, api, namespace, name, &prev).await;
    }

    // The upgrade path: a `Restore` written by a pre-fan-out operator carries
    // its single claim's state in the TOP-LEVEL status and nothing under
    // `claims`. Adopt it so an in-flight populate is driven under its old Job
    // name (never doubled) and a finished one is recorded as finished rather
    // than re-run. One legacy Job GET, only while `claims` is empty.
    let adopted = adopt_legacy_claims(ctx, restore, namespace, name, &claimants, &prev).await?;
    let prev = if adopted.is_empty() { prev } else { adopted };

    let live: Vec<(String, String)> = claimants
        .consumers
        .iter()
        .map(|c| (c.name_any(), c.uid().unwrap_or_default()))
        .collect();
    let plans: Vec<ClaimDrive> = live
        .iter()
        .map(|(claim, uid)| claim_drive(prev.get(claim), uid))
        .collect();

    // Records whose claimant is gone: reap their artifacts and null the entry.
    let live_names: Vec<&str> = live.iter().map(|(n, _)| n.as_str()).collect();
    let gone = reap_gone_claims(ctx, restore, namespace, &prev, &live_names).await?;

    // Skip the cluster-wide `PersistentVolume` LIST — the expensive read on this
    // path — on the steady heartbeat. Mirrors the pre-#443
    // `settled_over_bound_claim` shortcut, per claim; the rule is pure and
    // unit-tested in [`pass_is_all_quiet`].
    let rebound = if pass_is_all_quiet(&gone, &plans, &live, &prev, &claimants.primes) {
        std::collections::BTreeMap::new()
    } else {
        our_rebound_pvs(ctx, namespace).await?
    };

    let mut cache = PassCache::default();
    let mut next: std::collections::BTreeMap<String, kopiur_api::RestoreClaimStatus> =
        std::collections::BTreeMap::new();
    let mut events: Vec<(String, ClaimEvent)> = Vec::new();
    let mut finalizers: Vec<(String, PostPatchFinalize)> = Vec::new();
    let mut requeue = 600u64;

    for ((claim, uid), drive) in live.into_iter().zip(plans) {
        let outcome = drive_one_claim(
            ctx,
            restore,
            api,
            namespace,
            name,
            &claimants,
            &prev,
            &rebound,
            &mut cache,
            (&claim, &uid),
            drive,
        )
        .await?;
        requeue = requeue.min(outcome.requeue);
        if let Some(event) = outcome.event
            && claim_transition(prev.get(&claim), &outcome.record)
        {
            events.push((claim.clone(), event));
        }
        if let Some(finalize) = outcome.after_patch {
            finalizers.push((claim.clone(), finalize));
        }
        next.insert(claim, outcome.record);
    }

    // ONE status patch per pass — see [`fanout_status`] for why it is built in a
    // single pure place rather than at each decision point.
    let current = restore
        .status
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?;
    let status = fanout_status(restore, &prev, &next, &gone);
    io::patch_status_if_changed(api, name, current.as_ref(), status).await?;

    run_deferred_finalizers(ctx, namespace, name, finalizers).await?;

    for (claim, event) in events {
        tracing::warn!(%namespace, restore = %name, %claim, "{}", event.message);
        io::publish_warning_event(ctx, restore, event.reason, event.action, &event.message).await;
    }
    Ok(Action::requeue(std::time::Duration::from_secs(requeue)))
}

/// Tear down the artifacts of every handshake that completed on this pass —
/// ONLY after the end-of-pass status patch has landed.
///
/// The ordering is a data-safety property, not tidiness. `finalize_populator`
/// strips the rebind annotation — the only evidence the bound PV came from US —
/// so a crash between the strip and the status write would leave a restore that
/// genuinely ran looking like a claim we never touched: the next pass would read
/// it as `NothingToPopulate` and relabel it `TargetAlreadyBound` ("no restore
/// ran; delete the PVC"), over the volume holding the freshly restored data.
/// Deferring past the patch restores the pre-#443 ordering (status first,
/// teardown second) while keeping ONE conditions writer per pass; a crash before
/// this point simply replays the finalize.
async fn run_deferred_finalizers(
    ctx: &Context,
    namespace: &str,
    name: &str,
    finalizers: Vec<(String, PostPatchFinalize)>,
) -> Result<()> {
    for (claim, finalize) in finalizers {
        finalize_populator(
            ctx,
            namespace,
            &finalize.job,
            &finalize.prime,
            PrimePvAction::RestoreOriginalPolicy(&finalize.pv),
        )
        .await?;
        tracing::debug!(%namespace, restore = %name, %claim, "populator: finalized the handshake");
    }
    Ok(())
}

/// The pass for a populator NOTHING claims: reap the artifacts of every record
/// whose claimant is gone, then write the `AwaitingClaim=True` park AND those
/// records' `null`s in ONE patch.
///
/// Splitting it out keeps [`drive_populator_fanout`] to the N≥1 story, but the
/// pairing is the point: writing the park alone left the stale records standing,
/// so the next pass 30 s later re-reaped them and re-published
/// `OrphanedPrimePvcReaped`, forever, for every app torn down with its populator
/// left in place. With the nulls in the same body the second pass is a
/// server-side no-op under the deep merge predicate.
async fn park_unclaimed_populator(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    prev: &std::collections::BTreeMap<String, kopiur_api::RestoreClaimStatus>,
) -> Result<Action> {
    let gone = reap_gone_claims(ctx, restore, namespace, prev, &[]).await?;
    let (reason, msg) = claims_summary(&ClaimsAggregate::NoClaims);
    let current = restore
        .status
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?;
    io::patch_status_if_changed(
        api,
        name,
        current.as_ref(),
        awaiting_claim_status(restore, reason, &msg, &gone),
    )
    .await?;
    Ok(Action::requeue(std::time::Duration::from_secs(30)))
}

/// Reap (and name for nulling) every claim record whose claimant is no longer
/// live. Returns the record keys to delete from `status.claims`.
///
/// The PV is passed as `None` — [`PrimePvAction::Leave`] — DELIBERATELY. A PV we
/// rebound was already forced `Retain` at rebind time and keeps that setting, so
/// the restored data survives the claimant's deletion; the only residue is a
/// stale `populator-original-reclaim-policy` annotation on an orphan PV, which
/// [`our_rebound_pvs`] indexes harmlessly (the uid can never match again). Do
/// NOT "fix" this into [`PrimePvAction::RestoreOriginalPolicy`]: that puts the
/// class default (usually `Delete`) back on a `Released` volume and reaps the
/// data with it.
async fn reap_gone_claims(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
    prev: &std::collections::BTreeMap<String, kopiur_api::RestoreClaimStatus>,
    live_names: &[&str],
) -> Result<Vec<String>> {
    let mut gone = Vec::new();
    for (claim, record) in prev {
        if live_names.contains(&claim.as_str()) {
            continue;
        }
        if let Some(uid) = record.uid.as_deref() {
            reap_claim_artifacts(
                ctx,
                restore,
                namespace,
                claim,
                uid,
                record.reason.as_deref(),
                None,
            )
            .await?;
        }
        gone.push(claim.clone());
    }
    Ok(gone)
}

/// Seed `status.claims` from a LEGACY single-claim status, for the ONE claimant
/// the legacy handshake was actually about (#443's upgrade path).
///
/// The selection rule is pure and unit-tested ([`legacy_claimant`]): a surviving
/// uid-keyed `prime-<uid>` names the claim mid-handshake; failing that, exactly
/// one BOUND claimant is unambiguous however many claimants there are, because
/// the pre-fan-out driver could only ever populate one. That second arm is the
/// #443 reporter's own post-upgrade state — N claimants, one populated — where
/// adopting nothing relabels the populated PVC `TargetAlreadyBound`, "no restore
/// ran, delete the PVC", over the volume holding the restored data.
///
/// Gated by [`legacy_adoption`], so a brand-new `Restore`'s first claim does not
/// "adopt" the `Pending` park a zero-claim pass just wrote — while an in-flight
/// legacy populate, whose status the old operator never got to advance, is still
/// recognized by its live `prime-<uid>` and driven under its own Job name.
async fn adopt_legacy_claims(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
    name: &str,
    claimants: &Claimants,
    prev: &std::collections::BTreeMap<String, kopiur_api::RestoreClaimStatus>,
) -> Result<std::collections::BTreeMap<String, kopiur_api::RestoreClaimStatus>> {
    use k8s_openapi::api::batch::v1::Job;
    let mut adopted = std::collections::BTreeMap::new();
    let Some(status) = restore.status.as_ref() else {
        return Ok(adopted);
    };
    if !prev.is_empty() {
        return Ok(adopted);
    }
    // Exhaustive: a new adoption signal must decide what it means here.
    let evidence = match legacy_adoption(status, &claimants.consumers, &claimants.primes) {
        LegacyAdoption::PreFanoutStatus => "pre-fan-out status",
        LegacyAdoption::InFlightPrime => "a live prime for a live claimant",
        LegacyAdoption::No => return Ok(adopted),
    };
    let Some(consumer) = legacy_claimant(&claimants.consumers, &claimants.primes) else {
        return Ok(adopted);
    };
    // Drive an in-flight legacy populate under its OWN name, never a second
    // mover into the same prime.
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    let legacy_job = legacy_populate_job_name(name);
    let legacy = job_api
        .get_opt(&legacy_job)
        .await?
        .map(|_| legacy_job.as_str());
    if let Some(record) = adopt_legacy_claim(status, consumer, legacy) {
        tracing::info!(
            %namespace, restore = %name, claim = %consumer.name_any(), %evidence,
            "populator: adopted a pre-fan-out Restore status into status.claims"
        );
        adopted.insert(consumer.name_any(), record);
    }
    Ok(adopted)
}

/// Dispatch ONE claim on this pass: settle it, re-arm it, or drive its
/// handshake. Exhaustive over [`ClaimDrive`].
#[allow(clippy::too_many_arguments)]
async fn drive_one_claim(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    claimants: &Claimants,
    prev: &std::collections::BTreeMap<String, kopiur_api::RestoreClaimStatus>,
    rebound: &std::collections::BTreeMap<String, String>,
    cache: &mut PassCache,
    (claim, uid): (&str, &str),
    drive: ClaimDrive,
) -> Result<ClaimOutcome> {
    let record = prev.get(claim);
    match drive {
        // Terminal under the LIVE claimant's uid. Nothing to do — except finish
        // a reap whose best-effort delete did not land: a `Populated`/
        // `AlreadyBound` claim whose finalize stripped the PV annotation and
        // then died before deleting the prime, leaving an orphan nothing else
        // collects.
        //
        // Gated by [`settled_artifacts_reapable`], which is NARROWER than the
        // reaper's own reason gate: a `Failed` claim's mover Job and prime are
        // the evidence its own message points the user at ("see the Job/pod
        // logs"), restore movers carry no TTL, and pre-#443 both survived until
        // the user acted. They are reclaimed on re-arm or when the record is
        // dropped — i.e. when the user follows the documented remedy and
        // re-creates the claiming PVC.
        ClaimDrive::Settled => {
            let carried = record.cloned().unwrap_or_default();
            if claimants.prime_live(uid) && settled_artifacts_reapable(&carried) {
                reap_claim_artifacts(
                    ctx,
                    restore,
                    namespace,
                    claim,
                    uid,
                    carried.reason.as_deref(),
                    None,
                )
                .await?;
            }
            Ok(ClaimOutcome {
                record: carried,
                requeue: 600,
                event: None,
                after_patch: None,
            })
        }
        // The claimant was deleted and re-created: reap the dead claim's
        // artifacts under the RECORDED uid, then record the re-arm and come back
        // promptly to drive the fresh claim from a clean slate. This is how a
        // failed claim is retried.
        ClaimDrive::ReArm { stale_uid } => {
            reap_claim_artifacts(
                ctx,
                restore,
                namespace,
                claim,
                &stale_uid,
                record.and_then(|r| r.reason.as_deref()),
                rebound.get(&stale_uid).map(String::as_str),
            )
            .await?;
            Ok(ClaimOutcome {
                record: kopiur_api::RestoreClaimStatus {
                    uid: Some(uid.to_string()),
                    phase: Some(RestoreClaimPhase::Pending),
                    reason: Some(ClaimReason::ClaimRecreated.as_str().to_string()),
                    message: Some(format!(
                        "populator: claiming PVC `{claim}` was re-created (a new uid), so this \
                         claim re-arms: the previous claim's prime PVC, mover Job and volume were \
                         reaped and the source is resolved again."
                    )),
                    ..Default::default()
                },
                requeue: 5,
                event: None,
                after_patch: None,
            })
        }
        ClaimDrive::Drive => {
            let consumer = claimants
                .consumers
                .iter()
                .find(|c| c.name_any() == claim)
                .ok_or_else(|| Error::Invariant(format!("claimant `{claim}` vanished mid-pass")))?;
            let job = record
                .and_then(|r| r.job.clone())
                .unwrap_or_else(|| populate_job_name(name, uid));
            let job_reuse = if job == legacy_populate_job_name(name) {
                JobNameReuse::LegacyShared
            } else {
                JobNameReuse::Fresh
            };
            let cc = ClaimContext {
                consumer,
                consumer_name: claim.to_string(),
                uid: uid.to_string(),
                prime_name: prime_pvc_name(uid),
                job_name: job,
                job_reuse,
                prev: record,
            };
            drive_claim(ctx, restore, api, namespace, &cc, rebound, cache).await
        }
    }
}

/// Run ONE claim's populator handshake. Exhaustive over
/// [`PopulatorHandshake`], so a new binding state cannot compile until it is
/// handled per claim.
async fn drive_claim(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    cc: &ClaimContext<'_>,
    rebound: &std::collections::BTreeMap<String, String>,
    cache: &mut PassCache,
) -> Result<ClaimOutcome> {
    match populator_handshake(cc.consumer, rebound.get(&cc.uid).map(String::as_str)) {
        // The handover landed: restore the PV's reclaim policy, GC the
        // artifacts, record the claim as populated.
        PopulatorHandshake::FinalizeRebound { pv } => {
            claim_finalize_rebound(restore, api, cc, &pv).await
        }
        // Rebind issued; wait for the PV controller to bind our PV to the claim.
        PopulatorHandshake::AwaitingBind => Ok(ClaimOutcome {
            record: cc.record(
                RestoreClaimPhase::Rebinding,
                ClaimReason::PopulatingPrimePvc,
                format!(
                    "populator: the restored volume is rebound to claiming PVC \
                     `{}`; waiting for the PersistentVolume controller to bind it",
                    cc.consumer_name
                ),
            ),
            requeue: 5,
            event: None,
            after_patch: None,
        }),
        // #233 per claim: the claim is already bound, so there is nothing to
        // populate. Complete as a truthful no-op and reap anything that can
        // never be handed over.
        PopulatorHandshake::NothingToPopulate => {
            claim_already_bound(ctx, namespace, cc, None).await
        }
        // Our rebind was issued but a different PV won the claim: the handover
        // is lost. Same no-op ending, but our PV holds the restored data — keep
        // it (forced `Retain`).
        PopulatorHandshake::LostRebind { pv } => {
            claim_already_bound(ctx, namespace, cc, Some(&pv)).await
        }
        // Unbound claim, no rebind outstanding: populate it.
        PopulatorHandshake::Populate => {
            claim_populate(ctx, restore, api, namespace, cc, cache).await
        }
    }
}

/// The claim's prime PV is bound to it: settle the PV's reclaim policy, GC the
/// populate artifacts, and record the outcome.
///
/// The message branches on the claim's PINNED resolution, not on whether a
/// selection was dispatched: a deferred selector may itself have found nothing
/// under `Continue`. A real restore records `RestoreSucceeded`; deploy-or-restore
/// records `NoSnapshotContinue`, so an empty outcome is self-describing rather
/// than claiming data was written.
async fn claim_finalize_rebound(
    restore: &Restore,
    api: &Api<Restore>,
    cc: &ClaimContext<'_>,
    pv_name: &str,
) -> Result<ClaimOutcome> {
    // Re-read the pin FRESH: for a deferred restore the MOVER pins it in its own
    // terminal PATCH, which the cached `restore` may not yet reflect when a
    // PVC/PV bind event triggers this finalize.
    //
    // WHERE it pins depends on the Job, which is why `claim_finalize_pin` is an
    // exhaustive match on [`JobNameReuse`] rather than an `or_else` chain: a
    // `Fresh` claim's mover writes `status.claims.<pvc>.resolved` and must never
    // read the top level (that would let it inherit a sibling's pin), while an
    // adopted `LegacyShared` Job carries no `claimKey` and therefore pins the
    // TOP-LEVEL `status.resolved` — without that fallback an in-flight legacy
    // restore finalizes as "provisioned an empty volume" over a real one.
    let live_status = api
        .get_opt(&restore.name_any())
        .await?
        .and_then(|r| r.status);
    let live = claim_finalize_pin(
        cc.job_reuse,
        live_status
            .as_ref()
            .and_then(|s| s.claims.get(&cc.consumer_name).cloned())
            .and_then(|c| c.resolved),
        cc.prev.and_then(|p| p.resolved.clone()),
        live_status.and_then(|s| s.resolved),
    );
    let restored =
        live.as_ref().and_then(|r| r.resolution) == Some(kopiur_api::ResolutionOutcome::Snapshot);
    let (reason, message) = if restored {
        (
            ClaimReason::RestoreSucceeded,
            format!(
                "populator: restored the snapshot into claiming PVC `{}` and rebound the volume",
                cc.consumer_name
            ),
        )
    } else {
        (
            ClaimReason::NoSnapshotContinue,
            format!(
                "populator: no snapshot found; provisioned an empty volume for claiming PVC \
                 `{}` (deploy-or-restore)",
                cc.consumer_name
            ),
        )
    };
    let mut record = cc.record(RestoreClaimPhase::Populated, reason, message);
    record.resolved = live;
    // The handshake is over: the prime and the Job go with it.
    record.pvc_prime = None;
    record.job = None;
    // The teardown is DEFERRED past the end-of-pass status patch, and the order
    // is a data-safety property, not a tidiness one: `finalize_populator` strips
    // the rebind annotation — the only evidence the bound PV came from US — so
    // running it before the status write would leave a crash in between looking
    // like a claim we never touched, and the next pass would relabel it
    // `TargetAlreadyBound` ("no restore ran; delete the PVC") over the volume
    // holding the freshly restored data. See [`PostPatchFinalize`].
    Ok(ClaimOutcome {
        record,
        requeue: 600,
        event: None,
        after_patch: Some(PostPatchFinalize {
            job: cc.job_name.clone(),
            prime: cc.prime_name.clone(),
            pv: pv_name.to_string(),
        }),
    })
}

/// The claim is bound to a volume this restore did not hand it. Two very
/// different endings, told apart by `kept_pv`:
///
/// - `None` — the #233 no-op: the claim was already bound long before, nothing
///   was provisioned, no restore ran, the live volume was untouched.
/// - `Some(pv)` — a LOST rebind: a prime WAS provisioned and a restore DID run,
///   but a different volume won the claim, so the restored data sits on a PV we
///   force to `Retain` and hand to the admin.
///
/// A populate still RUNNING when the claim binds elsewhere is neither: it is a
/// HIJACK, and is failed honestly ([`fail_populate_hijacked`]).
async fn claim_already_bound(
    ctx: &Context,
    namespace: &str,
    cc: &ClaimContext<'_>,
    kept_pv: Option<&str>,
) -> Result<ClaimOutcome> {
    use k8s_openapi::api::batch::v1::Job;
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;

    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    // An object already being deleted is not an artifact to reap: re-issuing the
    // delete (and re-publishing the Event) every heartbeat while it sits
    // Terminating on a finalizer would be pure churn.
    let live = |meta: &kube::core::ObjectMeta| meta.deletion_timestamp.is_none();
    let prime_live = pvc_api
        .get_opt(&cc.prime_name)
        .await?
        .is_some_and(|p| live(&p.metadata));
    let job = job_api
        .get_opt(&cc.job_name)
        .await?
        .filter(|j| live(&j.metadata));
    let bound_volume = cc
        .consumer
        .spec
        .as_ref()
        .and_then(|s| s.volume_name.as_deref())
        .filter(|v| !v.is_empty());

    if let Some(job) = job.as_ref()
        && crate::snapshot::job_terminal_state(job).is_none()
    {
        return fail_populate_hijacked(ctx, namespace, cc, bound_volume).await;
    }

    let mut artifacts = Vec::new();
    if prime_live {
        artifacts.push(format!("prime PVC `{}`", cc.prime_name));
    }
    if job.is_some() {
        artifacts.push(format!("populate Job `{}`", cc.job_name));
    }
    let mut event = None;
    if !artifacts.is_empty() || kept_pv.is_some() {
        let action = match kept_pv {
            Some(pv) => PrimePvAction::ForceRetain(pv),
            None => PrimePvAction::Leave,
        };
        finalize_populator(ctx, namespace, &cc.job_name, &cc.prime_name, action).await?;
        event = Some(ClaimEvent {
            reason: ORPHANED_PRIME_REAPED_REASON,
            action: RECREATE_CLAIM_TO_RESTORE_ACTION,
            message: reaped_populate_artifacts_note(&artifacts, &cc.consumer_name, kept_pv),
        });
    }
    let (reason, message) = match kept_pv {
        Some(pv) => (
            ClaimReason::LostRebind,
            lost_rebind_message(&cc.consumer_name, pv),
        ),
        None => (
            ClaimReason::TargetAlreadyBound,
            target_already_bound_message(&cc.consumer_name, bound_volume),
        ),
    };
    let mut record = cc.record(RestoreClaimPhase::AlreadyBound, reason, message);
    record.pvc_prime = None;
    record.job = None;
    Ok(ClaimOutcome {
        record,
        requeue: 600,
        event,
        after_patch: None,
    })
}

/// The claiming PVC was bound out from under a populate that was STILL RUNNING:
/// some provisioner handed it a volume without honoring its `dataSourceRef`, so
/// the restore we are writing can never be delivered to it.
///
/// This is NOT the already-bound no-op (#233) even though it looks like one from
/// the API: there the claim was bound long before, by an earlier successful
/// restore, and the app is running on its own data. Here the app is about to come
/// up on somebody else's — probably empty — volume, so recording success would
/// tell `kubectl wait`, Flux and Argo that a restore landed when it did not.
/// Fail honestly, cancel the doomed run, and LEAVE the prime PVC in place: it
/// holds the data we were mid-way through writing.
///
/// What keeps that prime standing is [`ClaimReason::artifacts_reapable`], which
/// [`reap_claim_artifacts`] consults and which refuses exactly this reason. It
/// used to be the whole `Restore` short-circuiting on `Failed` at
/// [`phase_is_terminal_at_guard`]; the fan-out relaxes that guard for populators
/// (one failed claim must not stop its siblings), so the two changed together
/// and must stay together — a `Failed`/`PopulateHijacked` claim is `Settled` at
/// [`claim_drive`] and its artifacts are never reapable.
async fn fail_populate_hijacked(
    ctx: &Context,
    namespace: &str,
    cc: &ClaimContext<'_>,
    bound_volume: Option<&str>,
) -> Result<ClaimOutcome> {
    let msg = populate_hijacked_message(&cc.consumer_name, bound_volume);
    io::delete_mover_run(&ctx.client, namespace, &cc.job_name).await?;
    let mut record = cc.record(
        RestoreClaimPhase::Failed,
        ClaimReason::PopulateHijacked,
        msg.clone(),
    );
    // The Job is cancelled; the prime deliberately survives, so keep naming it.
    record.job = None;
    record.pvc_prime = Some(cc.prime_name.clone());
    Ok(ClaimOutcome {
        record,
        requeue: 600,
        after_patch: None,
        event: Some(ClaimEvent {
            reason: POPULATE_HIJACKED_REASON,
            action: RECREATE_CLAIM_TO_RESTORE_ACTION,
            message: msg,
        }),
    })
}

/// The source decision for one claim, or the parked record that stands in for
/// one. Closed so "no selection" can never be silently read as
/// deploy-or-restore.
enum ClaimResolution {
    /// Restore this selection into the claim's prime PVC.
    Restore(Box<RestoreSelection>),
    /// Deploy-or-restore: no snapshot matched and `Continue` chose an empty
    /// volume, so the freshly provisioned prime IS the result.
    EmptyVolume,
    /// The claim cannot proceed this pass; this record says why.
    Parked(Box<ClaimOutcome>),
}

/// Populate one unbound claim: resolve its own source, wait for a scheduling
/// hint if its class binds late, provision its prime PVC, run its mover, and
/// hand the volume over.
async fn claim_populate(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    cc: &ClaimContext<'_>,
    cache: &mut PassCache,
) -> Result<ClaimOutcome> {
    let mut resolved_pin = cc.prev.and_then(|p| p.resolved.clone());
    let mut source_path = cc.prev.and_then(|p| p.source_path.clone());
    let wait_started_at = cc
        .prev
        .and_then(|p| p.wait_started_at.clone())
        .or_else(|| Some(chrono::Utc::now().to_rfc3339()));

    let selection = match resolve_claim_source(
        ctx,
        restore,
        namespace,
        cc,
        cache,
        &mut resolved_pin,
        &mut source_path,
    )
    .await?
    {
        ClaimResolution::Parked(outcome) => {
            let mut outcome = *outcome;
            outcome.record.resolved = resolved_pin;
            outcome.record.source_path = source_path;
            outcome.record.wait_started_at = wait_started_at;
            return Ok(outcome);
        }
        ClaimResolution::Restore(sel) => Some(*sel),
        ClaimResolution::EmptyVolume => None,
    };

    // WaitForFirstConsumer, PER CLAIM (#443): before the fan-out one
    // unscheduled claimant parked the WHOLE Restore. A late-binding class only
    // provisions once a pod schedules the claim (the `selected-node` annotation
    // appears); the prime PVC is pinned to that node so its PV lands where the
    // workload will run. Siblings on an `Immediate` class — or already
    // scheduled — proceed regardless.
    let selected_node = cc
        .consumer
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(SELECTED_NODE_ANNOTATION).cloned());
    if selected_node.is_none() && cache.storage_class_is_wffc(ctx, cc.consumer).await? {
        let mut record = cc.record(
            RestoreClaimPhase::Pending,
            ClaimReason::AwaitingPodSchedule,
            format!(
                "populator: waiting for a pod to schedule claiming PVC `{}` \
                 (its StorageClass binds WaitForFirstConsumer, so there is no node to \
                 provision the prime volume on yet)",
                cc.consumer_name
            ),
        );
        record.resolved = resolved_pin;
        record.source_path = source_path;
        record.wait_started_at = wait_started_at;
        return Ok(ClaimOutcome {
            record,
            requeue: 15,
            event: None,
            after_patch: None,
        });
    }

    ensure_prime_pvc(
        ctx,
        restore,
        namespace,
        &cc.prime_name,
        cc.consumer,
        selected_node.as_deref(),
    )
    .await?;

    // Whether a mover of ours is still writing the prime: set on the arm that
    // observes it, never re-derived by comparing the phase we just wrote (which
    // would silently stop matching the day a new claim phase is added).
    let mut mover_in_flight = false;
    let mut record = if let Some(selection) = selection.as_ref() {
        let dispatch = RestoreDispatch {
            target_pvc: &cc.prime_name,
            source_target: kopiur_api::snapshot::PvcTargetRef {
                namespace: namespace.to_string(),
                name: cc.consumer_name.clone(),
            },
            claim_key: Some(&cc.consumer_name),
            job_reuse: cc.job_reuse,
        };
        match run_restore_mover(
            ctx,
            restore,
            api,
            namespace,
            &cc.job_name,
            selection,
            &dispatch,
        )
        .await?
        {
            MoverOutcome::Running { .. } => {
                mover_in_flight = true;
                cc.record(
                RestoreClaimPhase::Populating,
                ClaimReason::PopulatingPrimePvc,
                format!(
                    "populator: restoring the snapshot into prime PVC `{}` for claiming PVC `{}`",
                    cc.prime_name, cc.consumer_name
                ),
                )
            }
            MoverOutcome::Failed => {
                let mut record = cc.record(
                    RestoreClaimPhase::Failed,
                    ClaimReason::MoverJobFailed,
                    format!(
                        "the populator restore mover Job `{}` failed; see the Job/pod logs, fix \
                         the cause and re-create the claiming PVC `{}` to re-arm this claim \
                         (other claims continue)",
                        cc.job_name, cc.consumer_name
                    ),
                );
                record.pvc_prime = Some(cc.prime_name.clone());
                record.job = Some(cc.job_name.clone());
                record.resolved = resolved_pin;
                record.source_path = source_path;
                record.wait_started_at = wait_started_at;
                return Ok(ClaimOutcome {
                    record,
                    requeue: 120,
                    event: None,
                    after_patch: None,
                });
            }
            MoverOutcome::Wedged { message } => {
                let mut record = cc.record(
                    RestoreClaimPhase::Failed,
                    ClaimReason::MoverPodWedged,
                    message,
                );
                record.pvc_prime = Some(cc.prime_name.clone());
                // Name the Job explicitly: on the FIRST pass that observes the
                // wedge there is no prior record to carry it forward from, and
                // the whole point of the record is to say which pod is stuck.
                record.job = Some(cc.job_name.clone());
                record.resolved = resolved_pin;
                record.source_path = source_path;
                record.wait_started_at = wait_started_at;
                return Ok(ClaimOutcome {
                    record,
                    requeue: 120,
                    event: None,
                    after_patch: None,
                });
            }
            // Mover done: fall through to the rebind.
            MoverOutcome::Succeeded { .. } => cc.record(
                RestoreClaimPhase::Rebinding,
                ClaimReason::PopulatingPrimePvc,
                format!(
                    "populator: the restore mover finished; handing the volume to claiming PVC \
                     `{}`",
                    cc.consumer_name
                ),
            ),
        }
    } else {
        // Deploy-or-restore: no mover. The empty prime IS the result.
        cc.record(
            RestoreClaimPhase::Populating,
            ClaimReason::NoSnapshotContinue,
            format!(
                "populator: no snapshot found; provisioning an empty volume for claiming PVC \
                 `{}` (deploy-or-restore)",
                cc.consumer_name
            ),
        )
    };
    record.pvc_prime = Some(cc.prime_name.clone());
    record.job = selection.is_some().then(|| cc.job_name.clone());
    record.resolved = resolved_pin;
    record.source_path = source_path;
    record.wait_started_at = wait_started_at;

    // Still populating: leave the rebind for the pass that observes the mover go
    // terminal.
    if mover_in_flight {
        return Ok(ClaimOutcome {
            record,
            requeue: 15,
            event: None,
            after_patch: None,
        });
    }
    // `false` while the prime's PV hasn't been provisioned yet — come back soon.
    let issued =
        rebind_prime_to_consumer(ctx, namespace, &cc.prime_name, &cc.consumer_name, &cc.uid)
            .await?;
    if issued {
        record.phase = Some(RestoreClaimPhase::Rebinding);
    }
    Ok(ClaimOutcome {
        record,
        requeue: 5,
        event: None,
        after_patch: None,
    })
}

/// Resolve ONE claim's restore source — its own kopia source path, its own pin,
/// its own `waitTimeout` window.
///
/// The per-PVC path is the #443 cross-volume fix: a `fromPolicy` restore of a
/// selector policy used to resolve `source_path: None`, which becomes the kopia
/// filter `user@host:` and matches EVERY member path, so one claim could be
/// filled with another volume's data. Deriving the path from the claimant
/// through [`kopiur_api::expand::restore_source_path`] closes that; an ambiguous
/// policy FAILS CLOSED on this claim rather than guessing.
async fn resolve_claim_source(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
    cc: &ClaimContext<'_>,
    cache: &mut PassCache,
    resolved_pin: &mut Option<ResolvedRestore>,
    source_path: &mut Option<String>,
) -> Result<ClaimResolution> {
    // Pinned once, never re-resolved (ADR §4.6), per claim.
    if let Some(decision) = cc.prev.and_then(claim_pinned_decision) {
        return Ok(match decision {
            Resolution::Snapshot(id) => {
                ClaimResolution::Restore(Box::new(RestoreSelection::Snapshot(id)))
            }
            Resolution::Deferred(sel) => {
                ClaimResolution::Restore(Box::new(RestoreSelection::Resolve(sel)))
            }
            Resolution::Empty => ClaimResolution::EmptyVolume,
        });
    }

    let target = kopiur_api::snapshot::PvcTargetRef {
        namespace: namespace.to_string(),
        name: cc.consumer_name.clone(),
    };
    let window = claim_wait_window(cc.prev, restore, chrono::Utc::now().timestamp());
    let outcome =
        match resolve_snapshot(ctx, restore, namespace, window.anchor(), &target, cache).await? {
            SourceResolution::Resolved(outcome) => outcome,
            // Fail CLOSED: the policy names no single volume for this claim, so
            // restoring anything would be a guess — quite possibly with another
            // volume's data. Scoped to THIS claim; siblings are unaffected.
            SourceResolution::Ambiguous(msg) => {
                return Ok(ClaimResolution::Parked(Box::new(ClaimOutcome {
                    record: cc.record(
                        RestoreClaimPhase::Failed,
                        ClaimReason::SourcePathAmbiguous,
                        msg,
                    ),
                    requeue: 300,
                    event: None,
                    after_patch: None,
                })));
            }
        };
    match outcome {
        Some(ResolveOutcome::Pinned(res)) => {
            *source_path = res
                .identity
                .as_ref()
                .and_then(|i| i.source_path.clone())
                .or_else(|| source_path.clone());
            *resolved_pin = Some(ResolvedRestore {
                resolution: Some(ResolutionOutcome::Snapshot),
                kopia_snapshot_id: Some(res.kopia_snapshot_id.clone()),
                snapshot_ref: res.snapshot_ref,
                repository: None,
                pinned_at: Some(chrono::Utc::now().to_rfc3339()),
                identity: res.identity,
            });
            Ok(ClaimResolution::Restore(Box::new(
                RestoreSelection::Snapshot(res.kopia_snapshot_id),
            )))
        }
        // Object stores can't be listed in-process: the mover resolves
        // "latest"/offset/asOf in-Job and pins `status.claims.<pvc>.resolved`
        // itself, so the controller does NOT pin here.
        Some(ResolveOutcome::Deferred(sel)) => {
            *source_path = sel.source_path.clone();
            Ok(ClaimResolution::Restore(Box::new(
                RestoreSelection::Resolve(sel),
            )))
        }
        None => Ok(no_snapshot_for_claim(restore, cc, window, resolved_pin)),
    }
}

/// No snapshot matched this claim's source: keep waiting while its own
/// `waitTimeout` window is open, then honor `onMissingSnapshot` exhaustively.
///
/// The window is PER CLAIM ([`claim_wait_window`]) because claimants appear at
/// different times — a sibling created an hour after the first is owed its own
/// full window, not the remains of the first one's.
fn no_snapshot_for_claim(
    restore: &Restore,
    cc: &ClaimContext<'_>,
    window: WaitWindow,
    resolved_pin: &mut Option<ResolvedRestore>,
) -> ClaimResolution {
    let wait_timeout = restore
        .spec
        .policy
        .as_ref()
        .and_then(|p| p.wait_timeout.as_deref());
    let now = chrono::Utc::now().timestamp();
    if let Some(remaining) = wait_remaining_secs(window.anchor(), wait_timeout, now) {
        let (_, msg, requeue) = wait_park_report(window, wait_timeout, remaining);
        return ClaimResolution::Parked(Box::new(ClaimOutcome {
            record: cc.record(
                RestoreClaimPhase::Pending,
                ClaimReason::WaitingForSnapshot,
                format!("claiming PVC `{}`: {msg}", cc.consumer_name),
            ),
            requeue,
            event: None,
            after_patch: None,
        }));
    }
    match effective_on_missing(
        restore
            .spec
            .policy
            .as_ref()
            .and_then(|p| p.on_missing_snapshot),
        &restore.spec.source,
    ) {
        OnMissingSnapshot::Fail => ClaimResolution::Parked(Box::new(ClaimOutcome {
            record: cc.record(
                RestoreClaimPhase::Failed,
                ClaimReason::SnapshotNotFound,
                format!(
                    "no snapshot matched the restore source for claiming PVC `{}` within the \
                     waitTimeout window; fix spec.source (or create the missing snapshot) and \
                     re-create that PVC to re-arm this claim — other claims continue",
                    cc.consumer_name
                ),
            ),
            requeue: 300,
            event: None,
            after_patch: None,
        })),
        // Deploy-or-restore. We are about to provision the empty volume, so NOW
        // the decision is real and must be pinned: a snapshot that appears LATER
        // must never restore over it (ADR §4.6). Pinning any earlier would also
        // pin a pass that provisioned nothing.
        OnMissingSnapshot::Continue => {
            *resolved_pin = Some(ResolvedRestore {
                resolution: Some(ResolutionOutcome::NoSnapshot),
                pinned_at: Some(chrono::Utc::now().to_rfc3339()),
                ..Default::default()
            });
            ClaimResolution::EmptyVolume
        }
    }
}

/// The selected-node annotation a late-binding (`WaitForFirstConsumer`) PVC carries
/// once the scheduler picks a node for its first consuming pod.
const SELECTED_NODE_ANNOTATION: &str = "volume.kubernetes.io/selected-node";
/// Annotation kopiur stamps on a prime PV while it is temporarily forced to `Retain`
/// during the rebind — carries the PV's ORIGINAL reclaim policy so
/// [`finalize_populator`] can restore it once the consumer binds.
const PRIME_ORIGINAL_RECLAIM_ANNOTATION: &str =
    "kopiur.home-operations.com/populator-original-reclaim-policy";

/// What to do with the `PersistentVolume` our populator earmarked, when tearing the
/// populate artifacts down. Closed so the two very different "the consumer is bound"
/// endings can never be confused — one of them reclaims the volume, the other must not.
enum PrimePvAction<'a> {
    /// The consumer bound to OUR PV (the handover landed): restore the reclaim policy we
    /// stashed at rebind time and drop the annotation. The volume is now the consumer's.
    RestoreOriginalPolicy(&'a str),
    /// The handover was LOST (the consumer bound to a different PV). Our PV's `claimRef`
    /// points at a claim that is bound elsewhere, so the PV controller will mark it
    /// `Released` — and restoring the stashed policy (usually `Delete`) would then reap it,
    /// destroying the data we just restored. Force `Retain` and drop the annotation
    /// instead: the volume survives for the admin, and the stripped annotation makes the
    /// next pass see a plain already-bound claim (idempotent, no repeat Event).
    ForceRetain(&'a str),
    /// No PV of ours is involved (no rebind was ever issued).
    Leave,
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

/// Tear the populate artifacts down: settle the earmarked PV per [`PrimePvAction`], then
/// GC the populate Job/ConfigMap and any leftover prime PVC. Every delete tolerates an
/// absent object, so this is safe to call on a partially-completed handshake.
async fn finalize_populator(
    ctx: &Context,
    namespace: &str,
    populate_job: &str,
    prime_name: &str,
    pv: PrimePvAction<'_>,
) -> Result<()> {
    use k8s_openapi::api::batch::v1::Job;
    use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolume, PersistentVolumeClaim};

    // Exhaustive: the two bound endings settle the PV very differently, and conflating them
    // would either leak a volume or destroy the restored data.
    let policy = match pv {
        PrimePvAction::RestoreOriginalPolicy(pv_name) => Some((pv_name, None)),
        PrimePvAction::ForceRetain(pv_name) => Some((pv_name, Some("Retain".to_string()))),
        PrimePvAction::Leave => None,
    };
    if let Some((pv_name, forced)) = policy {
        let pv_api: Api<PersistentVolume> = Api::all(ctx.client.clone());
        if let Some(pv) = pv_api.get_opt(pv_name).await? {
            // `forced` wins (a lost rebind keeps the volume); otherwise put back the policy
            // stashed at rebind time. With neither, the PV never went through our rebind —
            // leave it exactly as it is.
            let target = forced.or_else(|| {
                pv.metadata
                    .annotations
                    .as_ref()
                    .and_then(|a| a.get(PRIME_ORIGINAL_RECLAIM_ANNOTATION))
                    .cloned()
            });
            if let Some(target) = target {
                let patch = serde_json::json!({
                    "metadata": { "annotations": { PRIME_ORIGINAL_RECLAIM_ANNOTATION: serde_json::Value::Null } },
                    "spec": { "persistentVolumeReclaimPolicy": target },
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

/// Terminal-success status for a DIRECT restore, branching on the pinned
/// `resolution`. The mover pins `Snapshot` (real restore) or `NoSnapshot` (a
/// deferred `Continue` that found nothing and left the target empty) BEFORE the
/// Job goes terminal, so the controller can stamp an accurate message:
/// - `Snapshot` → `RestoreSucceeded`, "data was written";
/// - `NoSnapshot` → self-describing `Resolved=True NoSnapshotContinue` + empty-volume message;
/// - unknown (best-effort pin AND terminal PATCH both lost) → a NEUTRAL message
///   that never falsely claims data was written. Mirrors the populator finalize.
fn restore_success_status(
    restore: &Restore,
    resolved: Option<&kopiur_api::restore::ResolvedRestore>,
) -> serde_json::Value {
    match resolved.and_then(|r| r.resolution) {
        Some(kopiur_api::ResolutionOutcome::NoSnapshot) => {
            let msg = "no snapshot matched the source; provisioned an empty target volume \
                       (deploy-or-restore)";
            let conditions = io::upsert_condition(
                &existing_conditions(restore),
                "Resolved",
                true,
                NO_SNAPSHOT_CONTINUE_REASON,
                msg,
                restore.metadata.generation,
            );
            restore_ready_status_on(
                restore,
                &conditions,
                RestorePhase::Completed,
                NO_SNAPSHOT_CONTINUE_REASON,
                msg,
            )
        }
        Some(kopiur_api::ResolutionOutcome::Snapshot) => restore_ready_status(
            restore,
            RestorePhase::Completed,
            crate::consts::RESTORE_POPULATED_REASON,
            "the restore mover completed; the snapshot data was written into the target",
        ),
        // Outcome unknown (the mover's best-effort status PATCHes were both lost):
        // report completion truthfully without claiming data was written.
        None => restore_ready_status(
            restore,
            RestorePhase::Completed,
            "RestoreSucceeded",
            "the restore mover Job completed; see status.logTail for what was restored",
        ),
    }
}

/// Drive a restore-with-explicit-target: create the restore mover Job (writing
/// into the target PVC), then track it to terminal.
///
/// `selection` is `None` for deploy-or-restore (`onMissingSnapshot: Continue` with no
/// matching snapshot): the target PVC is still ensured (so `target.pvc` is provisioned
/// empty rather than left missing), but the mover is skipped and the restore completes
/// cleanly with a fresh, empty volume. A `Some` selection is either a concrete id or an
/// in-Job selector the mover resolves.
async fn drive_direct_restore(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    selection: Option<&RestoreSelection>,
    source_target: &kopiur_api::snapshot::PvcTargetRef,
) -> Result<Action> {
    // Resolve the target PVC for the restore Job. DirectTarget is only reached for
    // an explicit PVC target (populator routes to AwaitingClaim in the reconcile
    // dispatch). Exhaustive over RestoreTarget so a new variant must be considered.
    let target_pvc = match &restore.spec.target {
        RestoreTarget::PvcRef(r) => r.name.clone(),
        // `target.pvc` means the operator CREATES the PVC (ADR §3.6) — without
        // this the mover Job references a claim nobody made and sits Pending
        // forever (FailedScheduling: persistentvolumeclaim not found). This also
        // provisions the empty volume for the deploy-or-restore (no-snapshot) case.
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

    let phase = restore.status.as_ref().and_then(|s| s.phase.as_ref());

    // Deploy-or-restore: no snapshot to write. The target PVC is ensured above (so a
    // `target.pvc` comes up empty rather than missing); a `pvcRef` already exists. Stamp
    // a terminal `Completed` via `restore_ready_status_on` (Ready=True) so the entry-guard
    // heal does NOT clobber this message with "the snapshot data was written" — there is no
    // mover here, so the controller is the sole writer (no two-writer race).
    let Some(selection) = selection else {
        if phase != Some(&RestorePhase::Completed) {
            let msg = "no snapshot found; provisioned an empty target volume (deploy-or-restore)";
            let conditions = io::upsert_condition(
                &existing_conditions(restore),
                "Resolved",
                true,
                NO_SNAPSHOT_CONTINUE_REASON,
                msg,
                restore.metadata.generation,
            );
            io::patch_status(
                api,
                name,
                restore_ready_status_on(
                    restore,
                    &conditions,
                    RestorePhase::Completed,
                    NO_SNAPSHOT_CONTINUE_REASON,
                    msg,
                ),
            )
            .await?;
        }
        return Ok(Action::requeue(std::time::Duration::from_secs(600)));
    };

    // The Job is named after the Restore and writes into the explicit target PVC;
    // the helper creates/tracks it, the phase writes stay here.
    let dispatch = RestoreDispatch {
        target_pvc: &target_pvc,
        source_target: source_target.clone(),
        // A direct restore keeps the classic top-level status shape: it is the
        // only writer, so there is nothing to key by claim.
        claim_key: None,
        // One-shot: its Job name is never re-used.
        job_reuse: JobNameReuse::Fresh,
    };
    match run_restore_mover(ctx, restore, api, namespace, name, selection, &dispatch).await? {
        MoverOutcome::Succeeded { duration_secs } => {
            if let Some(secs) = duration_secs {
                ctx.metrics.set_restore_duration(namespace, name, secs);
            }
            if phase != Some(&RestorePhase::Completed) {
                // A fresh read serves two purposes. (1) A deferred
                // (object-store/identity) resolution may have come up empty under
                // `Continue`: the mover pins the outcome to status.resolved before
                // its Job goes terminal, so the live object tells a real restore
                // from a deploy-or-restore-empty — without it the success message
                // would falsely claim "data was written". (2) The conditions BASE:
                // `restore_ready_status` rebuilds the conditions array from its
                // argument, and the watch-store copy driving this reconcile can
                // predate this controller's own launch-time condition patches
                // (SecurityContextInherited, the MoverPermitted clear) — on a
                // fast mover Job, building the terminal write on the stale copy
                // durably erased them (the condition-writers-clobber class,
                // caught by the recorded-identity restore e2e's 3s Job).
                let Some(live) = io::live_conditions_source(api, name, restore).await else {
                    return Ok(Action::requeue(std::time::Duration::from_secs(600)));
                };
                let resolved = live.status.as_ref().and_then(|s| s.resolved.clone());
                io::patch_status(api, name, restore_success_status(&live, resolved.as_ref()))
                    .await?;
            }
            Ok(Action::requeue(std::time::Duration::from_secs(600)))
        }
        MoverOutcome::Failed => {
            if phase != Some(&RestorePhase::Failed) {
                // Live conditions base for the same reason as the Succeeded arm.
                let Some(live) = io::live_conditions_source(api, name, restore).await else {
                    return Ok(Action::requeue(std::time::Duration::from_secs(120)));
                };
                io::patch_status(
                    api,
                    name,
                    restore_ready_status(
                        &live,
                        RestorePhase::Failed,
                        MOVER_JOB_FAILED_REASON,
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
            if created || phase != Some(&RestorePhase::Restoring) {
                // Live conditions base: on `created` this write follows the SAME
                // reconcile's inherit/compat condition patches (which re-read
                // live), so building on the reconcile-start copy would erase them
                // moments after they were written.
                let Some(live) = io::live_conditions_source(api, name, restore).await else {
                    return Ok(Action::requeue(std::time::Duration::from_secs(30)));
                };
                io::patch_status(
                    api,
                    name,
                    restore_ready_status(&live, target_phase, reason, msg),
                )
                .await?;
            }
            Ok(Action::requeue(std::time::Duration::from_secs(30)))
        }
        MoverOutcome::Wedged { message } => {
            if phase != Some(&RestorePhase::Failed) {
                io::patch_status(
                    api,
                    name,
                    restore_ready_status(
                        restore,
                        RestorePhase::Failed,
                        MOVER_POD_WEDGED_REASON,
                        &message,
                    ),
                )
                .await?;
            }
            Ok(Action::requeue(std::time::Duration::from_secs(120)))
        }
    }
}

/// What ONE restore mover dispatch is filling, and how it reports.
///
/// A struct rather than four more positional parameters because
/// `target_pvc` and `source_target` are DIFFERENT PVCs on the populator path
/// (the prime is written; the CLAIMANT is what the per-PVC kopia source path is
/// derived from, #443) and swapping them silently restores the wrong volume.
struct RestoreDispatch<'a> {
    /// The PVC the mover WRITES into, mounted read-write at `/restore`: the
    /// direct target, or a populator prime.
    target_pvc: &'a str,
    /// The PVC whose DATA this is. Same as `target_pvc` for a direct restore;
    /// the claiming PVC (never the prime) for a populator. The per-PVC source
    /// path and the recorded-identity catalog row are both keyed off it.
    source_target: kopiur_api::snapshot::PvcTargetRef,
    /// The `status.claims.<key>` the mover nests its status writes under;
    /// `None` keeps the classic top-level shape (direct restores).
    claim_key: Option<&'a str>,
    /// Whether this Job's name is re-used across runs.
    job_reuse: JobNameReuse,
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

/// Build + apply the restore mover Job named `job_name` (writing `selection` into
/// `target_pvc`, mounted read-write at `/restore`) and report its [`MoverOutcome`].
/// Idempotent: an existing Job is tracked to terminal, never re-applied. The caller
/// owns the status/phase writes. `selection` is a concrete id (anchor-healed) or an
/// in-Job selector the mover resolves.
/// The steady pass for a Restore already in a terminal phase (the entry guard):
/// heal the kstatus once, reap the finished run's work-spec ConfigMap, and
/// settle into the slow heartbeat.
///
/// The kstatus conditions come from the controller's transition patch in
/// `drive_direct_restore` — which the MOVER's own terminal `phase` stamp races
/// past in the common case (its in-cluster PATCH carries `phase:
/// Completed`/`Failed` + logTail/failure but no conditions; the Job-completion
/// reconcile then already sees the terminal phase and lands HERE, never in the
/// Job branch). Heal once: patch ONLY phase + observedGeneration + conditions
/// (`restore_ready_status` carries nothing else, so the merge preserves the
/// mover-written logTail/failure/progress and the pinned resolution).
/// Self-gated by `kstatus_settled_for`, so a healed Restore never re-patches.
async fn steady_terminal_restore(
    restore: &Restore,
    api: &Api<Restore>,
    name: &str,
    phase: &RestorePhase,
) -> Result<Action> {
    if !kstatus_settled_for(restore, phase) {
        // Live conditions base: this reconcile is typically triggered by the
        // MOVER's own terminal-phase PATCH, and the watch-store copy carrying it
        // can predate this controller's launch-time condition patches
        // (SecurityContextInherited, the MoverPermitted clear). Rebuilding the
        // conditions array from that stale copy durably erased them on fast
        // mover Jobs (the condition-writers-clobber class, caught by the
        // recorded-identity restore e2e's 3s Job) — the heal must build on the
        // live object.
        let Some(live) = io::live_conditions_source(api, name, restore).await else {
            return Ok(Action::requeue(std::time::Duration::from_secs(600)));
        };
        let status = if phase == &RestorePhase::Completed {
            // The mover wrote `phase: Completed` + `status.resolved` in one
            // PATCH, so `resolved` is observed here — distinguish a real
            // restore from a deploy-or-restore that came up empty.
            let resolved = live.status.as_ref().and_then(|s| s.resolved.clone());
            restore_success_status(&live, resolved.as_ref())
        } else {
            restore_ready_status(
                &live,
                phase.clone(),
                MOVER_JOB_FAILED_REASON,
                "the restore mover reported a terminal failure; see \
                 status.failure / status.logTail for the cause, fix it, and \
                 create a NEW Restore — a Failed Restore is terminal and \
                 never retries",
            )
        };
        io::patch_status(api, name, status).await?;
    }
    Ok(Action::requeue(std::time::Duration::from_secs(600)))
}

/// Classify an EXISTING restore mover Job into a [`MoverOutcome`], tearing
/// down a wedged mover. Split from [`run_restore_mover`] so both stay under
/// the complexity ratchet.
async fn observe_restore_mover(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
    job_name: &str,
    job: &k8s_openapi::api::batch::v1::Job,
) -> Result<MoverOutcome> {
    Ok(match crate::snapshot::job_terminal_state(job) {
        Some(true) => MoverOutcome::Succeeded {
            duration_secs: restore_job_duration_seconds(job),
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
                // BEST-EFFORT: an error must not abort before
                // `MoverOutcome::Wedged` reaches the caller — the Restore would
                // stay un-Failed and the retry would spawn a fresh wedged
                // mover, cycling instead of failing fast.
                let job_api: Api<k8s_openapi::api::batch::v1::Job> =
                    Api::namespaced(ctx.client.clone(), namespace);
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
    })
}

/// Map an absent restore TARGET PVC (seen while resolving mover co-location)
/// to its per-caller error (#382 M5 mapping table): the claim was ensured
/// moments earlier by this very reconcile — or is a user `pvcRef` that may
/// still be provisioning — so a 404 is a race and stays a transient
/// [`Error::MissingDependency`] retry. The Restore deliberately gets NO
/// gate/deadline machinery here.
pub(super) fn restore_target_pvc_race_error(pvc_ns: &str, pvc_name: &str) -> Error {
    Error::MissingDependency(format!(
        "restore target PVC `{pvc_ns}/{pvc_name}` was not found while resolving mover \
         co-location; it was just ensured (or may still be provisioning), so this is treated \
         as a race and retried automatically"
    ))
}

/// Take this restore's slot in the repository's mover-Job pool, for the whole
/// window between the launch decision and the Job's creation.
///
/// **Unconditional by construction.** A restore is a recovery in progress, so it
/// is admitted at and over every cap: there is no verdict here, no
/// `RepositorySlotAvailable` condition, no Event, and no requeue. That is a TYPE
/// property, not a runtime one — [`crate::pool::reserve_slot`] hands back the
/// guard directly, so this path never holds a value with a `Park` variant it
/// would have to dispose of via `unreachable!()` or a `_ =>`.
///
/// **What "never parked" does NOT mean is "never counted".** A restore that took
/// no reservation was invisible between its own admission and its Job appearing
/// in a LIST, and a `Snapshot` reconciling in that window read spare capacity and
/// launched beside it — two movers on a `maxConcurrentJobs: 1` repository. The
/// reservation makes the in-flight restore visible to concurrent admissions, so
/// it DISPLACES routine work (as documented) from the decision onward rather than
/// only once its Job exists.
///
/// `job_name` MUST be the name of the Job this dispatch actually applies — the
/// direct restore's (`{restore}`) or the populator's (`{restore}-populate`).
/// [`run_restore_mover`] threads ONE binding into both this call and
/// `apply_mover_objects`, which is what keeps the reservation matchable by the
/// ledger's observed-Job sweep; a second spelling could drift and leave a
/// reservation only the guard's `Drop` clears.
///
/// The pool key is the RESOLVED repository identity, for the same reason the
/// Job's pool label is: a restore often carries no `spec.repository` at all (it
/// derives one from a `Snapshot` or a policy), so the resolution is the only ref
/// guaranteed to be normalized — and a denormalized one would silently split the
/// repository's pool in two.
///
/// `Ok(None)` means the UNCAPPED default: no LIST, no lock, no ledger entry. It
/// never means "this restore was held".
async fn reserve_restore_slot(
    ctx: &Context,
    repo: &ResolvedRepository,
    namespace: &str,
    job_name: &str,
) -> Result<Option<crate::pool::AdmissionGuard>> {
    crate::pool::reserve_slot(
        ctx,
        &crate::naming::repo_label(&repo.repository_ref()),
        &crate::pool::job_key(namespace, job_name),
        crate::pool::PoolCaps {
            repo: kopiur_api::consts::effective_max_concurrent_jobs(repo.concurrency.as_ref()),
            global: ctx.max_concurrent_jobs,
        },
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_restore_mover(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    job_name: &str,
    selection: &RestoreSelection,
    dispatch: &RestoreDispatch<'_>,
) -> Result<MoverOutcome> {
    use k8s_openapi::api::batch::v1::Job;
    let target_pvc = dispatch.target_pvc;
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    if let Some(job) = job_api.get_opt(job_name).await? {
        // A Job being DELETED is not THIS run's outcome — but only where the Job name is
        // re-used across runs, which since #443 is ONLY a claim adopted from a pre-fan-out
        // status (the shared `<restore>-populate`). There, a reaped-then-immediately-
        // re-populated claim could read the outgoing Job's terminal `Succeeded` as its own
        // and rebind a still-empty prime. Wait for the delete to land, then create a fresh Job.
        //
        // Every other dispatch is `Fresh` — a direct restore is one-shot, and a fanned-out
        // populate is named after the CLAIMANT's uid — so a terminating Job there IS this
        // run's Job (typically TTL-reaped after succeeding), and ignoring its terminal state
        // would drop the outcome and re-run a full restore over a target PVC the workload may
        // already be writing to. Exhaustive, so a third naming scheme must decide this.
        let outgoing = match dispatch.job_reuse {
            JobNameReuse::LegacyShared => job.metadata.deletion_timestamp.is_some(),
            JobNameReuse::Fresh => false,
        };
        if outgoing {
            return Ok(MoverOutcome::Running { created: false });
        }
        return observe_restore_mover(ctx, restore, namespace, job_name, &job).await;
    }

    let target_path = "/restore".to_string();
    // Status patches and the work-spec `target_ref` reference the Restore itself; the
    // Job/ConfigMap/cache are named after `job_name` (`<restore>-populate` for the
    // populator path) so the two paths never collide.
    let restore_name = restore.name_any();
    let name = restore_name.as_str();

    // Resolve the repository for the restore Job. A missing `tls.caBundleRef`
    // ConfigMap surfaces as the structural CredentialsAvailable gate
    // (MISSING_CA_BUNDLE_GATE): the retry is transient, but a ConfigMap nobody
    // creates never self-heals, and the park at `Pending` must be visible to
    // doctor (#359).
    let repo = match resolve_restore_repository(ctx, restore, namespace).await {
        Ok(repo) => repo,
        Err(Error::MissingCaBundle(msg)) => {
            let existing = restore
                .status
                .as_ref()
                .map(|s| s.conditions.clone())
                .unwrap_or_default();
            let conditions = io::upsert_gate(
                &existing,
                &kopiur_api::gates::MISSING_CA_BUNDLE_GATE,
                &msg,
                restore.metadata.generation,
            );
            io::patch_status(
                api,
                name,
                serde_json::json!({ "phase": "Pending", "conditions": conditions }),
            )
            .await?;
            io::publish_missing_ca_bundle_event(ctx, restore, &msg).await;
            return Err(Error::MissingCaBundle(msg));
        }
        Err(e) => return Err(e),
    };

    // Repository mover-Job pool RESERVATION.
    //
    // Placed HERE — right after the repository resolves (the first point the
    // pool key and the caps are even known) and BEFORE `ensure_mover_identity`
    // mints anything — so the reservation covers the WHOLE window between this
    // decision and `apply_mover_objects` far below.
    //
    // `_slot` is bound with a NAME (not `_`) so it lives to the end of this
    // function rather than dropping at the end of the statement; every exit
    // from here on — the `?`s, the early returns, an unwinding panic — releases
    // it exactly once. See [`reserve_restore_slot`] for why it is unconditional.
    let _slot = reserve_restore_slot(ctx, &repo, namespace, job_name).await?;

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
        ctx.mover_role_kind.as_str(),
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
            let conditions = io::upsert_gate(
                &existing,
                &kopiur_api::gates::MISSING_SERVICE_ACCOUNT_GATE,
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

    // Resolve the restore mover's EFFECTIVE security context once (explicit, inherited
    // from a workload pod via `inheritSecurityContextFrom`, or replayed from the
    // backup's RECORDED identity via `inheritSecurityContextFrom.snapshot`). Both the
    // gate and the Job use it, so an inherited/recorded root context is gated like an
    // explicit one.
    //
    // The recorded-identity source resolves FIRST (a no-op `None` unless the mover uses
    // the `snapshot` variant): the Snapshot CR's `status.recorded` — direct for
    // `snapshotRef`, via the CR-catalog search for `fromPolicy`/`identity`. Its
    // `MissingDependency` holds are permanent-SHAPED (a pre-feature snapshot or a
    // catalog scan that has not landed yet), so they are surfaced as an explicit
    // `SecurityContextInherited=False`/`MissingRecordedIdentity` condition + Event and
    // requeued on the slow structural cadence (300s) instead of the fast transient one
    // — a bare `?` here would hot-loop a generic error with no condition.
    // The concrete kopia id this dispatch restores, when there is one — the meta
    // reader uses it to select the SAME catalog row the resolution pinned, so the
    // replayed identity always belongs to the snapshot actually being restored.
    let dispatch_pinned_id = match selection {
        RestoreSelection::Snapshot(id) => Some(id.as_str()),
        RestoreSelection::Resolve(_) => None,
    };
    let recorded_source = match snapshot_recorded_source(
        ctx,
        restore,
        namespace,
        dispatch_pinned_id,
        &dispatch.source_target,
    )
    .await
    {
        Ok(s) => s,
        Err(Error::MissingDependency(msg)) => {
            report_missing_recorded_identity(restore, api, &msg, ctx).await?;
            return Err(Error::MissingRecordedIdentity(msg));
        }
        Err(e) => return Err(e),
    };
    // Restore has no backup *source* PVC; `pvcConsumer` is backup-only (validator-rejected
    // for restore), so pass None.
    let mover_security = match io::resolve_mover_security_contexts(
        &ctx.client,
        namespace,
        restore.spec.mover.as_ref(),
        None,
        recorded_source.as_ref(),
    )
    .await
    {
        Ok(s) => s,
        // The snapshot-inherit hold: the matched CR carries no `status.recorded` AND the
        // explicit context pins no fallback identity (the resolver reports a `Fallback`
        // outcome instead when it does — that path stays reported as today). Same
        // explicit condition + slow requeue as above. Guarded on `recorded_source` so a
        // live-pod inherit's MissingDependency (workload scaled to zero) keeps its
        // existing fast-transient propagation.
        Err(Error::MissingDependency(msg)) if recorded_source.is_some() => {
            report_missing_recorded_identity(restore, api, &msg, ctx).await?;
            return Err(Error::MissingRecordedIdentity(msg));
        }
        Err(e) => return Err(e),
    };
    let (effective_sc, effective_pod_sc) = mover_security.contexts.clone();
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
        let conditions = io::upsert_gate(
            &existing,
            &kopiur_api::gates::PRIVILEGED_MOVER_GATE,
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

    // A restore that falls back to its explicit context is NOT tracking the workload it named —
    // report it, exactly as a backup does.
    //
    // Placed HERE, not at the resolve site, for two reasons. (1) After the privileged-mover
    // gate: reporting before it emits a Warning Event about fallback behavior for a run the
    // gate then refuses, which never happens. (2) After the "clear stale MoverPermitted" block:
    // that block rebuilds `conditions` from the reconcile-start copy, so a condition written
    // before it is erased — and then re-written and re-Evented on the next reconcile, forever.
    //
    // Matched exhaustively (CLAUDE.md: "prefer an `enum` + exhaustive `match` over `if let` /
    // `_ =>` catch-alls in reconcile paths") so a new `InheritOutcome` variant cannot be
    // silently dropped here — the very failure mode this feature exists to remove.
    match &mover_security.outcome {
        io::InheritOutcome::Fallback { reason } => {
            report_restore_inherit_fallback(namespace, restore, reason, ctx).await;
        }
        // Recorded-identity inherit reports POSITIVELY too (unlike `workloadSelector`
        // restores, which stay Fallback-only): provable provenance is this mode's entire
        // value, so the condition must name the Snapshot, the recorded uid, and the
        // provenance — and a record that contributed nothing beyond the hardened
        // baseline must warn, not claim success.
        io::InheritOutcome::InheritedFromSnapshot { snapshot, uid, src } => {
            let meta = recorded_source.as_ref().and_then(|s| s.meta.as_ref());
            let verdict = recorded_inherit_verdict(
                snapshot,
                *uid,
                *src,
                meta.and_then(|m| m.gid),
                meta.and_then(|m| m.fs_group),
            );
            report_restore_recorded_inherit(namespace, restore, &verdict, ctx).await;
        }
        // The backup-only `InheritPinnedNoUid` warning has no restore counterpart on purpose:
        // an fsGroup-only inherit is a *blessed* restore shape (`RestoreBasis::FsGroupMatch`),
        // because the target is a fresh read-write volume the kubelet does apply fsGroup to.
        // Warning there would flag a configuration the operator itself certifies as compatible.
        io::InheritOutcome::Inherited { .. } | io::InheritOutcome::NotRequested => {}
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
        &io::CredsPrefix::restore(name),
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
            let conditions = io::upsert_gate(
                &existing,
                &kopiur_api::gates::MISSING_CREDENTIALS_GATE,
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
    let creds_secrets = io::plain_creds(creds.names);

    let identity = MoverIdentity {
        username: "restore".into(),
        hostname: namespace.to_string(),
        source_path: target_path.clone(),
    };
    // Carry the Restore CRD's options (ADR §4.6, M2 flag sweep) through to the
    // mover so kopia honors them. `None` lets kopia use its defaults.
    let flags = restore_flags(&restore.spec.options);
    // Effective cache config (repository cacheDefaults overlaid by this restore's
    // mover.cache, ADR §3.1) drives both the connect budgets and the cache volume.
    let effective_cache = crate::cache::effective_cache(
        &repo,
        restore.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
    );
    let cache = crate::cache::cache_tuning(effective_cache.as_ref());
    // Stable anchors so the mover can heal a stale pinned id (kopia rewrites the
    // manifest id on pin). Only meaningful for a concrete id; a Resolve selector
    // lists fresh in-Job, so it carries no anchor.
    let anchor = match selection {
        RestoreSelection::Snapshot(_) => restore_source_anchor(ctx, restore, namespace).await,
        RestoreSelection::Resolve(_) => SnapshotAnchor::default(),
    };
    let work_spec = MoverWorkSpec {
        version: 2,
        operation: Operation::Restore(RestoreOp {
            source: selection.clone(),
            target_path: target_path.clone(),
            anchor,
            ignore_permission_errors: flags.ignore_permission_errors,
            write_files_atomically: flags.write_files_atomically,
            parallel: flags.parallel,
            write_sparse_files: flags.write_sparse_files,
            skip_owners: flags.skip_owners,
            skip_permissions: flags.skip_permissions,
            skip_times: flags.skip_times,
            overwrite_files: flags.overwrite_files,
            overwrite_directories: flags.overwrite_directories,
            overwrite_symlinks: flags.overwrite_symlinks,
            ignore_errors: flags.ignore_errors,
            skip_existing: flags.skip_existing,
            delete_extra: flags.delete_extra,
        }),
        identity,
        repository: restore_connect(&repo)?,
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            kind: "Restore".to_string(),
            name: name.to_string(),
            namespace: namespace.to_string(),
            // #443: a fanned-out populate nests every status write under
            // `status.claims.<pvc>` (and omits `phase`, which the controller
            // owns). Without it N concurrent movers would clobber ONE
            // top-level `status.resolved`, and a claim could be handed the
            // snapshot a SIBLING's mover pinned.
            claim_key: dispatch.claim_key.map(str::to_string),
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
        // Exhaustive over the outcome (no `_ =>`, #382 M5): the target PVC was
        // just ensured above (or is the user's own `pvcRef`, which may still be
        // provisioning), so a 404 here is a race — keep the transient
        // MissingDependency retry, never the Snapshot-side terminal machinery
        // (Restore's parking precedent, `waitTimeout`/`onMissing`, is about
        // snapshots, not PVCs).
        let decision = match io::resolve_source_colocation(
            &ctx.client,
            namespace,
            target_pvc,
            resolved_mover.source_colocation,
        )
        .await?
        {
            io::ColocationOutcome::Resolved(decision) => decision,
            io::ColocationOutcome::SourcePvcAbsent {
                namespace: pvc_ns,
                name: pvc_name,
            } => return Err(restore_target_pvc_race_error(&pvc_ns, &pvc_name)),
        };
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
        image_pull_policy: ctx.mover_pull_policy(),
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
        // moverDefaults.podLabels/podAnnotations, applied to EVERY mover pod
        // (podLabels also to the Job; podAnnotations pod-only).
        pod_labels: resolved_mover.pod_labels.clone(),
        pod_annotations: resolved_mover.pod_annotations.clone(),
        labels: {
            let mut labels =
                io::child_labels(&[(crate::consts::OP_LABEL, crate::consts::OP_RESTORE)]);
            mover_identity.decorate_labels(&mut labels);
            // Pool membership: a restore mover counts toward its repository's
            // `spec.concurrency.maxConcurrentJobs`. Keyed off the RESOLVED
            // repository identity — a restore often has no `spec.repository`
            // at all (it derives one from a Snapshot or a policy), so the
            // resolution is the only ref guaranteed to be normalized, and a
            // denormalized one would split the repository's pool.
            labels.extend(crate::pool::repo_pool_label(
                crate::pool::MoverJobKind::Restore,
                &repo.repository_ref(),
            ));
            labels
        },
        // Restore writes INTO the target PVC, mounted read-write at /restore.
        source_volume: Some(VolumeMountSpec::pvc(target_pvc, target_path, false)),
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
    io::apply_mover_objects(&ctx.client, namespace, job_name, None, &job).await?;
    let source_label = match selection {
        RestoreSelection::Snapshot(id) => format!("snapshot {id}"),
        RestoreSelection::Resolve(sel) => {
            format!("resolve {}@{}", sel.username, sel.hostname)
        }
    };
    tracing::info!(restore = %name, job = %job_name, source = %source_label, "created restore mover Job");
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

/// The outcome of resolving a restore source in the controller (ADR §4.6): a
/// concrete snapshot the controller can pin now (`snapshotRef` / explicit id), or
/// a selector handed to the mover Job because the backend can't be listed
/// in-process (`fromPolicy` / `identity`-without-id). Exactly one — externally
/// modeled as an enum so the reconcile core must handle both.
/// What resolving a restore's source produced — or why it could not be
/// resolved at all.
///
/// [`Self::Ambiguous`] is deliberately an OUTCOME, not an [`Error`]: the
/// `Restore` and the `SnapshotPolicy` are each perfectly valid, they just name
/// no single volume for this target (a selector policy whose sources disagree on
/// `sourcePathStrategy`/`sourcePathOverride`, or share one override — #443).
/// That is reported on the affected CLAIM's own record, so one ambiguous claim
/// does not have to fail the whole reconcile of a fanned-out populator; and
/// modelling it as a variant means every caller must decide what to do with it
/// instead of a `?` quietly turning a cross-volume hazard into a generic retry.
enum SourceResolution {
    /// Resolved: a concrete outcome, or `None` for "no snapshot matched".
    Resolved(Option<ResolveOutcome>),
    /// No single per-PVC kopia source path could be derived. The payload is the
    /// ready-to-surface what / why / fix.
    Ambiguous(String),
}

#[derive(Debug, Clone)]
enum ResolveOutcome {
    /// Resolved here; pin `status.resolved` and dispatch with a concrete id.
    Pinned(ResolvedSource),
    /// Deferred to the mover; dispatch with this selector and let the mover pin.
    Deferred(RestoreSelector),
}

/// Stable identity anchors for the restore's snapshot, so the mover can
/// self-heal a STALE pinned id: kopia rewrites a snapshot's manifest id on pin,
/// so a `snapshotRef`/`identity` restore of a snapshot pinned before the
/// re-stamp fix points at a deleted manifest. The anchors (source path + start
/// time, plus username/hostname when known) survive the rewrite; identity
/// additionally guards against re-resolving to a DIFFERENT source's snapshot
/// at the same path (the same PVC subpath repeats across namespaces, and, in a
/// shared repository, across clusters).
///
/// `snapshotRef` reads the referenced `Snapshot` CR's recorded status (which
/// keeps the correct identity/timing even when its id went stale). `identity`/
/// `fromPolicy` use the pinned `status.resolved.identity` (best-effort, no
/// start time — those resolve from a live list at admission), falling back to
/// the raw `IdentitySource` when nothing is pinned yet. Empty when nothing is
/// recorded yet (the mover then restores by id only).
async fn restore_source_anchor(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
) -> SnapshotAnchor {
    if let RestoreSource::SnapshotRef(r) = &restore.spec.source {
        let ns = r.namespace.as_deref().unwrap_or(namespace);
        let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), ns);
        if let Ok(Some(snap)) = api.get_opt(&r.name).await {
            let st = snap.status.as_ref();
            let identity = st.and_then(|s| s.snapshot.as_ref()).map(|s| &s.identity);
            return SnapshotAnchor {
                source_path: identity
                    .and_then(|i| i.source_path.clone())
                    .unwrap_or_default(),
                start_time: st
                    .and_then(|s| s.timing.as_ref())
                    .and_then(|t| t.start_time.clone()),
                username: identity.map(|i| i.username.clone()),
                hostname: identity.map(|i| i.hostname.clone()),
            };
        }
    }
    let resolved_identity = restore
        .status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .and_then(|r| r.identity.as_ref());
    let source_path = resolved_identity
        .and_then(|i| i.source_path.clone())
        .or_else(|| match &restore.spec.source {
            RestoreSource::Identity(id) => id.source_path.clone(),
            _ => None,
        })
        .unwrap_or_default();
    let (username, hostname) = match resolved_identity {
        Some(i) => (Some(i.username.clone()), Some(i.hostname.clone())),
        None => match &restore.spec.source {
            RestoreSource::Identity(id) => (Some(id.username.clone()), Some(id.hostname.clone())),
            _ => (None, None),
        },
    };
    SnapshotAnchor {
        source_path,
        start_time: None,
        username,
        hostname,
    }
}

/// Resolve the recorded-identity source for `inheritSecurityContextFrom.snapshot`,
/// BEFORE the mover security-context resolution. `None` unless this restore's mover
/// uses the `snapshot` variant (every other shape needs no Snapshot CR read).
///
/// - `source.snapshotRef` reads the referenced `Snapshot` CR directly (mirrors
///   [`restore_source_anchor`]) and hands back its `status.recorded` — possibly
///   absent, which the resolver turns into the actionable hold (or a reported
///   `Fallback` when the explicit context pins an identity).
/// - `source.fromPolicy`/`source.identity` run the CR-catalog search
///   ([`select_recorded_source`]): derive the kopia identity triple (fromPolicy:
///   re-resolved from the LIVE SnapshotPolicy exactly like source resolution;
///   identity: the user-supplied triple), LIST the Snapshot CRs where that source's
///   rows live, and pick the recorded-carrying row honoring
///   `asOf`/`offset`/`snapshotID`. This is what makes the GitOps re-bootstrap
///   declarative: apply Repository + SnapshotPolicy + Restore, and the restore
///   resolves the moment the catalog scan materializes a discovered row.
///
/// Namespace choice for the search: fromPolicy rows live in the POLICY's namespace
/// (schedules create Snapshots there, and the catalog materializes a discovered row
/// in the namespace named by the identity's hostname — the policy namespace by
/// construction); a raw `identity` source searches the Restore's own namespace (the
/// renamed-cluster escape hatch restores into the namespace being rebuilt).
///
/// Every `Err(MissingDependency)` from here is a permanent-shaped hold the caller
/// reports as `MissingRecordedIdentity` and requeues slowly.
/// Whether this restore's mover replays the identity RECORDED on its snapshot
/// (`inheritSecurityContextFrom: { snapshot: {} }`). Pure; shared by the meta
/// reader and by `resolve_snapshot`, which must then pin data + identity to the
/// SAME snapshot (the P1 coherence rule).
fn snapshot_inherit_active(restore: &Restore) -> bool {
    use kopiur_api::common::InheritSecurityContextFrom;
    matches!(
        restore
            .spec
            .mover
            .as_ref()
            .and_then(|m| m.inherit_security_context_from.as_ref()),
        Some(InheritSecurityContextFrom::Snapshot(_))
    )
}

/// `pinned_id` is the concrete kopia snapshot id the CURRENT dispatch restores
/// (from the in-memory `RestoreSelection`), when there is one. For
/// `fromPolicy`/`identity` sources it is the coherence key: `resolve_snapshot`
/// pinned data + identity from one CR-catalog row, and passing the id back in
/// here selects exactly that row again — the recorded meta can never come from
/// a different (newer/other) snapshot than the one being restored, even if the
/// catalog changed between reconciles.
async fn snapshot_recorded_source(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
    pinned_id: Option<&str>,
    target: &kopiur_api::snapshot::PvcTargetRef,
) -> Result<Option<io::SnapshotRecordedSource>> {
    if !snapshot_inherit_active(restore) {
        return Ok(None);
    }
    // Exhaustive over the source: each arm must state how recorded meta is found.
    match &restore.spec.source {
        RestoreSource::SnapshotRef(r) => {
            let ns = r.namespace.as_deref().unwrap_or(namespace);
            let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), ns);
            let snap = api.get_opt(&r.name).await?.ok_or_else(|| {
                Error::MissingDependency(format!(
                    "snapshotRef Snapshot `{ns}/{}` not found — if this cluster was \
                     re-bootstrapped, the catalog scan materializes discovered/adopted \
                     rows; the Restore holds until it appears. (Or use source.fromPolicy/\
                     identity, which search the catalog by identity instead of by name.)",
                    r.name
                ))
            })?;
            Ok(Some(io::SnapshotRecordedSource {
                snapshot: format!("{ns}/{}", r.name),
                meta: snap.status.as_ref().and_then(|s| s.recorded.clone()),
            }))
        }
        RestoreSource::FromPolicy(c) => {
            use kopiur_api::SnapshotPolicy;
            let cfg_ns = c.namespace.as_deref().unwrap_or(namespace);
            let cfg_api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), cfg_ns);
            let config = cfg_api.get_opt(&c.name).await?.ok_or_else(|| {
                Error::MissingDependency(format!("SnapshotPolicy {cfg_ns}/{}", c.name))
            })?;
            let repo = resolve_restore_repository(ctx, restore, namespace).await?;
            // The per-PVC identity, not the policy's pathless one (#443): the CR
            // catalog is searched BY identity, so a pathless triple matches the
            // newest row of ANY member of a selector policy — the same
            // cross-volume hazard the restore itself has, by a different route.
            let triple = from_policy_identity(
                &config,
                cfg_ns,
                repo.identity_defaults.as_ref(),
                c.source_path.as_deref(),
                target,
            )
            .map_err(Error::Validation)?;
            // A pinned data id (the resolution pinned from this same catalog) selects
            // exactly that row; asOf/offset only apply on the un-pinned first pass.
            let row = search_recorded_source(
                ctx,
                cfg_ns,
                &triple,
                c.as_of.as_deref().filter(|_| pinned_id.is_none()),
                if pinned_id.is_some() { 0 } else { c.offset },
                pinned_id,
            )
            .await?;
            Ok(Some(io::SnapshotRecordedSource {
                snapshot: format!("{cfg_ns}/{}", row.name),
                meta: Some(row.meta),
            }))
        }
        RestoreSource::Identity(id) => {
            let triple = kopiur_api::common::ResolvedIdentity {
                username: id.username.clone(),
                hostname: id.hostname.clone(),
                source_path: id.source_path.clone(),
            };
            let effective_pin = pinned_id.or(id.snapshot_id.as_deref());
            let row = search_recorded_source(
                ctx,
                namespace,
                &triple,
                id.as_of.as_deref().filter(|_| effective_pin.is_none()),
                if effective_pin.is_some() {
                    0
                } else {
                    id.offset.unwrap_or(0)
                },
                effective_pin,
            )
            .await?;
            Ok(Some(io::SnapshotRecordedSource {
                snapshot: format!("{namespace}/{}", row.name),
                meta: Some(row.meta),
            }))
        }
    }
}

/// The `Snapshot` CR row a `fromPolicy`/`identity` restore selected from the CR
/// catalog: the recorded identity it supplies AND the exact kopia snapshot it
/// belongs to, so identity and data are pinned to the SAME snapshot.
#[derive(Debug, Clone)]
struct RecordedRow {
    /// The CR's name (in the searched namespace).
    name: String,
    /// `status.snapshot.kopiaSnapshotID` — what the restore must pin as its data.
    kopia_snapshot_id: String,
    /// `status.snapshot.identity` — pinned as the resolution's provenance/anchor.
    identity: kopiur_api::common::ResolvedIdentity,
    /// `status.recorded` — the identity the restore mover replays.
    meta: kopiur_api::recorded::RecordedSnapshotMeta,
}

/// LIST the namespace's Snapshot CRs and run the pure [`select_recorded_source`]
/// over them; no match is the `MissingRecordedIdentity` hold (the scan may still be
/// running). Thin IO wrapper so the selection semantics stay unit-tested.
async fn search_recorded_source(
    ctx: &Context,
    ns: &str,
    triple: &kopiur_api::common::ResolvedIdentity,
    as_of: Option<&str>,
    offset: i64,
    snapshot_id: Option<&str>,
) -> Result<RecordedRow> {
    let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), ns);
    let rows = api.list(&kube::api::ListParams::default()).await?.items;
    // `asOf` was validated at admission with the same parser; re-parse defensively
    // (an unparseable stored value degrades to "no cutoff" rather than erroring —
    // the shared validator rejects it separately on the reconcile path).
    let cutoff = as_of
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc));
    select_recorded_source(triple, cutoff, offset, snapshot_id, &rows).ok_or_else(|| {
        Error::MissingDependency(format!(
            "no Snapshot CR carrying recorded identity (`status.recorded`) matches \
             `{}@{}:{}`{} in namespace `{ns}` yet — the catalog scan may still be running \
             (it materializes discovered rows and backfills recorded metadata when the \
             kopia snapshot carries the `kopiur-meta` tag); the Restore holds until one \
             appears",
            triple.username,
            triple.hostname,
            triple.source_path.as_deref().unwrap_or("*"),
            snapshot_id
                .map(|id| format!(" (kopia snapshot `{id}`)"))
                .unwrap_or_default(),
        ))
    })
}

/// §3.6 CR-catalog search, pure: pick the `Snapshot` CR whose recorded identity a
/// `fromPolicy`/`identity` restore should inherit.
///
/// Candidates must match the kopia identity triple on `status.snapshot.identity`
/// (username/hostname exact; a `sourcePath` in the triple must match exactly, an
/// absent one matches any path — the same "absent matches any" the mover's own
/// selector uses) and must carry `status.recorded` (a row without it cannot supply
/// an identity — the backfill adds it when the kopia snapshot carries the tag).
/// Discovered rows match too: identity comparison never touches `policyRef`.
///
/// Selection mirrors `kopiur_kopia::selection::{filter_as_of, pick_offset}` against
/// `status.timing`: a `snapshotID` pins the exact row by
/// `status.snapshot.kopiaSnapshotID` (admission forbids combining it with
/// `asOf`/`offset`); otherwise candidates are ordered newest-first by timing
/// (`endTime`, else `startTime`; undated rows sort last and are excluded under a
/// cutoff, since membership cannot be proven), `asOf` keeps rows at-or-before the
/// cutoff, and `offset` steps back from the newest (negative clamps to 0;
/// out-of-range yields `None`).
///
/// Coherence (the P1 review finding): the returned row carries its
/// `kopiaSnapshotID` so the caller can pin the DATA selection to the very
/// snapshot that supplied the identity. `resolve_snapshot` pins it for
/// `fromPolicy`/unpinned-`identity` sources whenever the `snapshot` inherit is
/// active — the mover never runs its own independent live-listing selection
/// there, so catalog lag or repository changes can no longer restore snapshot
/// B's data under snapshot A's recorded uid/gid/fsGroup.
fn select_recorded_source(
    triple: &kopiur_api::common::ResolvedIdentity,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
    offset: i64,
    snapshot_id: Option<&str>,
    rows: &[Snapshot],
) -> Option<RecordedRow> {
    let identity_matches = |row: &kopiur_api::snapshot::SnapshotInfo| {
        row.identity.username == triple.username
            && row.identity.hostname == triple.hostname
            && match &triple.source_path {
                Some(p) => row.identity.source_path.as_deref() == Some(p.as_str()),
                None => true,
            }
    };
    // A row's selection timestamp: timing endTime, else startTime (RFC3339).
    let row_time = |snap: &Snapshot| -> Option<chrono::DateTime<chrono::Utc>> {
        let timing = snap.status.as_ref()?.timing.as_ref()?;
        let raw = timing
            .end_time
            .as_deref()
            .or(timing.start_time.as_deref())?;
        chrono::DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    let mut candidates: Vec<(&Snapshot, Option<chrono::DateTime<chrono::Utc>>)> = rows
        .iter()
        .filter(|snap| {
            let Some(status) = snap.status.as_ref() else {
                return false;
            };
            status.recorded.is_some() && status.snapshot.as_ref().is_some_and(&identity_matches)
        })
        .map(|snap| (snap, row_time(snap)))
        .collect();

    let pick = |snap: &Snapshot| {
        let status = snap.status.as_ref()?;
        let info = status.snapshot.as_ref()?;
        Some(RecordedRow {
            name: snap.name_any(),
            kopia_snapshot_id: info.kopia_snapshot_id.clone(),
            identity: kopiur_api::common::ResolvedIdentity {
                username: info.identity.username.clone(),
                hostname: info.identity.hostname.clone(),
                source_path: info.identity.source_path.clone(),
            },
            meta: status.recorded.clone()?,
        })
    };

    if let Some(id) = snapshot_id {
        // An exact pin: `asOf`/`offset` are admission-rejected alongside it.
        return candidates
            .iter()
            .find(|(snap, _)| {
                snap.status
                    .as_ref()
                    .and_then(|s| s.snapshot.as_ref())
                    .is_some_and(|s| s.kopia_snapshot_id == id)
            })
            .and_then(|(snap, _)| pick(snap));
    }
    if let Some(cutoff) = as_of {
        // Undated rows cannot prove "at or before the cutoff" — exclude them.
        candidates.retain(|(_, t)| t.is_some_and(|t| t <= cutoff));
    }
    // Newest-first; undated rows last; name as the deterministic tie-break.
    candidates.sort_by(|(a, ta), (b, tb)| tb.cmp(ta).then_with(|| a.name_any().cmp(&b.name_any())));
    let idx = usize::try_from(offset.max(0)).ok()?;
    candidates.get(idx).and_then(|(snap, _)| pick(snap))
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

    // Re-read rather than trust the copy this reconcile started with: an earlier step may have
    // already patched `SecurityContextInherited` (the InheritFallback report) into status, and a
    // `conditions` patch REPLACES the whole array — computing it from the stale copy would erase
    // that condition.
    let name = restore.name_any();
    let api: Api<Restore> = Api::namespaced(ctx.client.clone(), namespace);
    let Some(live) = io::live_conditions_source(&api, &name, restore).await else {
        return; // deleted mid-reconcile
    };
    let existing = live
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
    let current = serde_json::to_value(&live.status).ok();
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
    // `Unknown` (legacy stored) modes never reach here: `validate_restore` runs at
    // reconcile entry and rejects them per-CR with the value quoted.
    let access_modes: Vec<String> = if template.access_modes.is_empty() {
        vec!["ReadWriteOnce".to_string()]
    } else {
        template
            .access_modes
            .iter()
            .map(|m| m.mode_str().to_string())
            .collect()
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

/// The kopia identity a `fromPolicy` restore of `target` must read under — the
/// #443 cross-volume fix, in one place so the resolution path and the
/// recorded-identity catalog search can never disagree.
///
/// [`kopiur_api::expand::restore_source_path`] derives the PER-PVC source path
/// (override → the plain source's own path → the selector strategy applied to
/// `target`), and [`crate::snapshot_policy::config_identity_for_path`] resolves
/// the username/hostname exactly as the BACKUP side did, with that path pinned.
/// A policy whose selector sources name no single volume for `target` yields
/// `Err(message)` — fail closed, because a pathless identity matches the newest
/// snapshot of ANY member and would fill the volume with another volume's data.
/// The error is the ready-to-surface what/why/fix text, deliberately NOT an
/// [`Error`]: it is a DOMAIN outcome (a valid `Restore` plus a valid
/// `SnapshotPolicy` that together name no single volume), reported on the
/// affected claim's own record rather than as a reconcile failure.
fn from_policy_identity(
    config: &kopiur_api::SnapshotPolicy,
    config_namespace: &str,
    defaults: Option<&kopiur_api::IdentityDefaults>,
    source_path_override: Option<&str>,
    target: &kopiur_api::snapshot::PvcTargetRef,
) -> std::result::Result<kopiur_api::common::ResolvedIdentity, String> {
    let path =
        restore_source_path(config, source_path_override, target).map_err(|e| e.to_string())?;
    crate::snapshot_policy::config_identity_for_path(
        config,
        config_namespace,
        defaults,
        path.path(),
    )
    .map_err(|e| e.to_string())
}

/// Resolve the restore's source (ADR §4.6). `snapshotRef`/`identity.snapshotID`
/// resolve to a concrete id the controller pins ([`ResolveOutcome::Pinned`]);
/// `fromPolicy`/`identity`-without-id defer the by-identity snapshot listing to
/// the mover Job ([`ResolveOutcome::Deferred`]) because in-process listing only
/// works for filesystem repos, and the mover reaches every backend — EXCEPT when
/// `inheritSecurityContextFrom: { snapshot: {} }` is active: the mover then
/// replays a snapshot's recorded identity, so data and identity must pin from the
/// SAME CR-catalog row ([`pin_from_recorded_catalog`]; the P1 coherence rule) and
/// those sources pin too. Returns `None` ONLY for a `snapshotRef` whose
/// `Snapshot` CR has no resolved snapshot yet (the caller applies `waitTimeout` +
/// `onMissingSnapshot`); deferred sources never return `None` — the mover applies
/// `onMissingSnapshot` in-Job.
async fn resolve_snapshot(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
    wait_anchor: i64,
    target: &kopiur_api::snapshot::PvcTargetRef,
    cache: &mut PassCache,
) -> Result<SourceResolution> {
    use kopiur_api::common::{ObjectRef, ResolvedIdentity};
    // Selector policy is the same for both deferred arms: the per-mode default
    // onMissing unless the spec overrides it, and the waitTimeout window as an
    // ABSOLUTE deadline anchored at `wait_anchor` — the instant the window OPENED
    // (`ensure_wait_anchor`), so the in-Job wait matches the snapshotRef path and is
    // stable across pod retries — polled by the mover until that wall-clock instant.
    let on_missing = effective_on_missing(
        restore
            .spec
            .policy
            .as_ref()
            .and_then(|p| p.on_missing_snapshot),
        &restore.spec.source,
    );
    let wait_deadline = wait_deadline_rfc3339(
        wait_anchor,
        restore
            .spec
            .policy
            .as_ref()
            .and_then(|p| p.wait_timeout.as_deref()),
    );
    match &restore.spec.source {
        RestoreSource::SnapshotRef(r) => {
            let ns = r.namespace.as_deref().unwrap_or(namespace);
            let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), ns);
            let backup = api.get_opt(&r.name).await?;
            Ok(SourceResolution::Resolved(
                backup
                    .and_then(|b| b.status)
                    .and_then(|s| s.snapshot)
                    .map(|s| {
                        ResolveOutcome::Pinned(ResolvedSource {
                            kopia_snapshot_id: s.kopia_snapshot_id,
                            snapshot_ref: Some(ObjectRef {
                                name: r.name.clone(),
                                namespace: Some(ns.to_string()),
                            }),
                            // Record the referenced snapshot's identity (provenance, and
                            // the source-path anchor used to self-heal a stale id).
                            identity: Some(s.identity),
                        })
                    }),
            ))
        }
        RestoreSource::Identity(id) => {
            // An explicit snapshot id wins — pin it directly. Otherwise defer the
            // listing to the mover (works on every backend).
            if let Some(sid) = &id.snapshot_id {
                return Ok(SourceResolution::Resolved(Some(ResolveOutcome::Pinned(
                    ResolvedSource {
                        kopia_snapshot_id: sid.clone(),
                        snapshot_ref: None,
                        identity: Some(ResolvedIdentity {
                            username: id.username.clone(),
                            hostname: id.hostname.clone(),
                            source_path: id.source_path.clone(),
                        }),
                    },
                ))));
            }
            let triple = ResolvedIdentity {
                username: id.username.clone(),
                hostname: id.hostname.clone(),
                source_path: id.source_path.clone(),
            };
            if snapshot_inherit_active(restore) {
                // COHERENCE (the P1 review finding): the mover replays the identity
                // recorded on a snapshot, so the DATA must be that same snapshot. A
                // deferred selection would let the mover pick from the LIVE listing
                // while the identity came from the CR catalog — catalog lag or
                // repository changes could then restore snapshot B's data under
                // snapshot A's uid/gid/fsGroup. Pin both from one CR-catalog row.
                return pin_from_recorded_catalog(
                    ctx,
                    restore,
                    namespace,
                    namespace,
                    &triple,
                    id.as_of.as_deref(),
                    id.offset.unwrap_or(0),
                )
                .await
                .map(|o| SourceResolution::Resolved(Some(o)));
            }
            Ok(SourceResolution::Resolved(Some(ResolveOutcome::Deferred(
                RestoreSelector {
                    username: id.username.clone(),
                    hostname: id.hostname.clone(),
                    source_path: id.source_path.clone(),
                    as_of: id.as_of.clone(),
                    offset: id.offset.unwrap_or(0),
                    on_missing,
                    wait_deadline: wait_deadline.clone(),
                },
            ))))
        }
        RestoreSource::FromPolicy(c) => {
            // Resolve the identity from the SnapshotPolicy, then defer the listing.
            // The referents come from the PASS cache: every claim of a fanned-out
            // populator derives its path from the SAME policy, so N claims must not
            // mean N SnapshotPolicy GETs plus N repository resolutions per requeue.
            let referents = cache.policy_referents(ctx, restore, namespace, c).await?;
            let cfg_ns = referents.namespace.as_str();
            // The PER-PVC identity (#443). A selector policy resolves no path at
            // all through `config_identity`, and `RestoreSelector.source_path:
            // None` becomes the kopia filter `user@host:` — an EMPTY path that
            // matches every member, so one volume could be filled with another's
            // data. Fails closed when the policy names no single volume.
            let identity = match from_policy_identity(
                &referents.config,
                cfg_ns,
                referents.identity_defaults.as_ref(),
                c.source_path.as_deref(),
                target,
            ) {
                Ok(identity) => identity,
                Err(message) => return Ok(SourceResolution::Ambiguous(message)),
            };
            if snapshot_inherit_active(restore) {
                // Same coherence rule as the `identity` arm above: identity and data
                // pin from ONE CR-catalog row, never two independent selections.
                return pin_from_recorded_catalog(
                    ctx,
                    restore,
                    namespace,
                    cfg_ns,
                    &identity,
                    c.as_of.as_deref(),
                    c.offset,
                )
                .await
                .map(|o| SourceResolution::Resolved(Some(o)));
            }
            Ok(SourceResolution::Resolved(Some(ResolveOutcome::Deferred(
                RestoreSelector {
                    username: identity.username,
                    hostname: identity.hostname,
                    source_path: identity.source_path,
                    as_of: c.as_of.clone(),
                    offset: c.offset,
                    on_missing,
                    wait_deadline: wait_deadline.clone(),
                },
            ))))
        }
    }
}

/// Pin a `fromPolicy`/unpinned-`identity` + `snapshot`-inherit restore from ONE
/// CR-catalog row: the row supplies BOTH the kopia snapshot id (the data) and,
/// later via [`snapshot_recorded_source`] re-selecting the same id, the recorded
/// identity — so the two can never diverge (the P1 coherence rule). Under catalog
/// lag this restores the newest *catalogued* snapshot rather than the newest live
/// one, coherently; the scan converges the catalog. No matching row is the
/// `MissingRecordedIdentity` hold (condition + Event + slow structural requeue).
async fn pin_from_recorded_catalog(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
    search_ns: &str,
    triple: &kopiur_api::common::ResolvedIdentity,
    as_of: Option<&str>,
    offset: i64,
) -> Result<ResolveOutcome> {
    match search_recorded_source(ctx, search_ns, triple, as_of, offset, None).await {
        Ok(row) => Ok(ResolveOutcome::Pinned(ResolvedSource {
            kopia_snapshot_id: row.kopia_snapshot_id,
            snapshot_ref: Some(kopiur_api::common::ObjectRef {
                name: row.name,
                namespace: Some(search_ns.to_string()),
            }),
            identity: Some(row.identity),
        })),
        Err(Error::MissingDependency(msg)) => {
            let api: Api<Restore> = Api::namespaced(ctx.client.clone(), namespace);
            report_missing_recorded_identity(restore, &api, &msg, ctx).await?;
            Err(Error::MissingRecordedIdentity(msg))
        }
        Err(e) => Err(e),
    }
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
    // FromPolicy: the policy's repository SET governs (multi-repo fan-out,
    // #368). The pure selection rule is shared with the M9 webhook mirror
    // (`kopiur_api::snapshot_policy::select_restore_repository`):
    // - explicit `spec.repository` must be a MEMBER of the set (audit m4 — a
    //   typo'd ref must not silently read a repository the recipe never wrote
    //   to), and resolves relative to the RESTORE's namespace as an explicit
    //   ref always has;
    // - no explicit + single-repo → the policy's one ref (as before, resolved
    //   in the policy's namespace);
    // - no explicit + multi-repo → fail closed, naming every valid choice.
    if let RestoreSource::FromPolicy(c) = &restore.spec.source {
        use kopiur_api::SnapshotPolicy;
        let cfg_ns = c.namespace.as_deref().unwrap_or(namespace);
        let cfg_api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), cfg_ns);
        let config = cfg_api.get_opt(&c.name).await?.ok_or_else(|| {
            Error::MissingDependency(format!("SnapshotPolicy {cfg_ns}/{}", c.name))
        })?;
        let selected = kopiur_api::snapshot_policy::select_restore_repository(
            &config.spec,
            &c.name,
            cfg_ns,
            restore.spec.repository.as_ref(),
            namespace,
        )
        .map_err(|e| Error::Validation(e.to_string()))?;
        let base_ns = if restore.spec.repository.is_some() {
            namespace
        } else {
            cfg_ns
        };
        // Launch-side (restore mover inputs) — store-backed point read (#382 M2).
        return io::resolve_repository_ref_cached(ctx, &selected, base_ns).await;
    }
    // Explicit `spec.repository` wins for the other sources. Honors `kind`
    // (namespaced vs. ClusterRepository) via the shared resolver (ADR §5.5).
    if let Some(rref) = &restore.spec.repository {
        return io::resolve_repository_ref_cached(ctx, rref, namespace).await;
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
        return io::resolve_repository_ref_cached(ctx, &rref, snap_ns).await;
    }
    Err(Error::Validation(
        "restore with source.identity requires spec.repository (snapshotRef and fromPolicy \
         sources derive it; a raw identity has nothing to derive from)"
            .into(),
    ))
}

/// The [`RepositoryRef`] a restore's mover will connect to, plus the namespace the
/// ref resolves relative to — WITHOUT resolving the repository object itself. The
/// readiness gate's cheap peer of [`resolve_restore_repository`], sharing its
/// derivation rule (explicit `spec.repository` wins; `snapshotRef`/`fromPolicy`
/// derive from the referent; a raw `identity` source has nothing to derive from).
///
/// Returns a [`RepoRefLookup`], not an `Option`: "the ref cannot be determined"
/// is several different situations and they do NOT get the same treatment
/// (issue #393 — the old `Option` collapsed a `SnapshotPolicy` that was never
/// applied together with a snapshot row the restore is legitimately waiting for,
/// and let the gate fall through unverified for both). Each `get_opt` result is
/// matched rather than chained through `and_then`, so "row missing" stays
/// distinct from "row present but underivable"; the classification itself is
/// pure and unit-tested ([`classify_snapshot_lookup`], [`classify_policy_lookup`]).
///
/// Still deliberately NON-FATAL: nothing here errors. The gate decides what each
/// shape means.
async fn restore_repository_ref(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
) -> Result<RepoRefLookup> {
    if let Some(rref) = &restore.spec.repository {
        return Ok(RepoRefLookup::Derived(rref.clone(), namespace.to_string()));
    }
    match &restore.spec.source {
        RestoreSource::SnapshotRef(sref) => {
            // Resolved relative to the SNAPSHOT's namespace (an absent ref
            // namespace means "same as the snapshot", not "same as the restore")
            // — the same base `resolve_restore_repository` uses.
            let snap_ns = sref.namespace.as_deref().unwrap_or(namespace);
            let snap_api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), snap_ns);
            let snap = snap_api.get_opt(&sref.name).await?;
            Ok(classify_snapshot_lookup(snap.as_ref(), snap_ns))
        }
        RestoreSource::FromPolicy(c) => {
            use kopiur_api::SnapshotPolicy;
            let cfg_ns = c.namespace.as_deref().unwrap_or(namespace);
            let cfg_api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), cfg_ns);
            let cfg = cfg_api.get_opt(&c.name).await?;
            Ok(classify_policy_lookup(cfg.as_ref(), cfg_ns, &c.name))
        }
        // A raw identity source has nothing to derive from, and `spec.repository`
        // is REQUIRED for it (checked above and refused downstream): a spec
        // problem, never a missing object.
        RestoreSource::Identity(_) => Ok(RepoRefLookup::NotDerivable),
    }
}

/// Where the repository-readiness gate leaves a restore on this pass — a
/// TRI-STATE, because "the repository is not Ready" and "kopiur cannot tell
/// whether it is Ready" are different answers with different consequences for
/// the `waitTimeout` window (issue #393).
///
/// [`Self::Held`] and [`Self::Undetermined`] both park and both requeue; they
/// are separate variants because they are separate *facts*, written with
/// different reasons, and only one of them is a verified statement about a
/// repository. Both stop the reconcile BEFORE [`ensure_wait_anchor`], so
/// neither opens the window.
///
/// [`WaitWindow`] deliberately gains no variant for this: a parked restore never
/// reaches the window machinery at all, so there is no third window state to
/// model — the window is simply not open yet.
///
/// **Match this enum EXHAUSTIVELY.** Never `matches!(…)` it and never add a
/// `_ =>` arm — a fourth outcome must force its caller to decide, at compile
/// time, whether it proceeds or parks. COMPILER-ONLY guard: `cargo xtask
/// check-phases` scans only the `*Phase` enums in `kopiur-api`.
enum RepositoryGate<'a> {
    /// The gate does not hold this restore: reconcile continues (and the
    /// `waitTimeout` window may open).
    ///
    /// It carries **the `Restore` the rest of the pass must use** — borrowed
    /// unchanged in the ordinary case, and an owned copy holding the cleared
    /// conditions when this pass cleared a stale `ReferentAvailable=False`
    /// ([`restore_with_conditions`]). Returning the object rather than a
    /// "…and also remember to apply this" side value is deliberate: the caller
    /// cannot obtain a `&Restore` to continue with WITHOUT taking the carried
    /// one, so the clear cannot be silently dropped by a later edit. Dropping it
    /// would let the unconditional downstream gate parks re-write the stale
    /// `False` and alternate two writes forever — see [`restore_with_conditions`].
    ///
    /// [`CarriedRestore`], not `Cow<Restore>`: a `Cow`'s owned variant is stored
    /// inline, and `Restore` is large enough that it would bloat every value of
    /// this enum past `clippy::large_enum_variant` (denied workspace-wide) while
    /// the sibling variants are one `Action` each. `CarriedRestore` boxes only
    /// its owned arm, so this stays pointer-sized AND the borrowed path keeps
    /// costing nothing.
    Proceed(CarriedRestore<'a>),
    /// VERIFIED not ready: the repository object exists and its phase is not
    /// `Ready` (the backend is unreachable). Park + requeue.
    Held(Action),
    /// UNDETERMINED: an object the readiness check needs does not exist — the
    /// `Repository`/`ClusterRepository` itself, or the `fromPolicy`
    /// `SnapshotPolicy` its ref is derived from. Park + requeue; nothing is
    /// claimed about the backend.
    Undetermined(Action),
}

/// The Restore peer of the Snapshot reconciler's repository-readiness gate: hold a
/// not-yet-launched restore in `Pending` (`Ready=False`/`RepositoryNotReady`,
/// non-terminal) while its repository is not `Ready`, and requeue on the same 15s
/// cadence. Returns a [`RepositoryGate`] the caller matches exhaustively.
///
/// Two semantic notes, both deliberate:
/// - **`spec.mode: ReadOnly` is a different axis.** The Snapshot reconciler's mode
///   gate refuses backups on a read-only repository and explicitly says "Restores
///   remain allowed (the Restore reconciler does not gate on mode)" — that stays
///   true; a read-only repo serves restores. Readiness (`status.phase == Ready`)
///   is REACHABILITY: an unreachable backend fails restores exactly like backups,
///   hence this gate.
/// - **`waitTimeout` interaction.** Clearing this gate is what OPENS the wait window
///   ([`ensure_wait_anchor`], #380): time parked here does NOT consume it, and the
///   window is both opened and evaluated only against a Ready repository. Once open
///   it is an ABSOLUTE deadline anchored at `status.waitStartedAt` (see
///   `resolve_snapshot` — spec'd so the in-Job wait is stable across pod retries), so
///   a repository outage can never fail a restore — or fake a `Continue` empty-volume
///   outcome — by silently burning the window while the backend is down.
///
/// **Tri-state (issue #393).** "Ready", "not Ready" and "cannot tell" are three
/// answers, and the gate used to give two: every cannot-tell shape fell through
/// as if the repository had been verified, so `ensure_wait_anchor` stamped the
/// window against nothing. Now a MISSING referent — the `Repository` object
/// itself, or the `fromPolicy` `SnapshotPolicy` its ref derives from — parks as
/// [`RepositoryGate::Undetermined`] with its own reason, no anchor stamped. Two
/// cannot-tell shapes still deliberately proceed: a `snapshotRef` whose
/// `Snapshot` row does not exist yet (that IS what the window waits for, and
/// `onMissingSnapshot: Fail` must be able to fire for a typo'd ref) and a
/// referent that exists but derives no single repository (a spec bug for
/// downstream validation to fail closed on). See [`RepoRefLookup`].
///
/// **Wake-up contract.** The `Repository`/`ClusterRepository` → `Restore` watches
/// cover only an explicit `spec.repository` (`watch::repository_to_restores`);
/// there is NO `SnapshotPolicy` → `Restore` watch. An `Undetermined` park
/// therefore un-parks on its own 15s requeue, not on an event — which is why
/// every `Undetermined` arm must return a requeue. (Adding the missing watch is
/// a possible follow-up; the requeue is correct either way.)
///
/// Only a restore that has NOT launched its mover Job is gated
/// ([`restore_awaiting_launch`]); a `Restoring` restore's live Job is tracked to
/// terminal, never re-gated, mirroring the Snapshot reconciler's ordering. (Narrow
/// crash window: a Job created moments before the controller died — before the
/// `Restoring` patch landed — is re-gated as `Pending`/`Resolving`; harmless, the
/// Job runs to terminal on its own and is observed once the gate opens.)
async fn gate_on_repository_readiness<'a>(
    ctx: &Context,
    restore: &'a Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
) -> Result<RepositoryGate<'a>> {
    if !restore_awaiting_launch(restore.status.as_ref().and_then(|s| s.phase.as_ref())) {
        return proceed_past_gate(restore, api, name).await;
    }
    let (rref, base_ns) = match restore_repository_ref(ctx, restore, namespace).await? {
        RepoRefLookup::Derived(rref, base_ns) => (rref, base_ns),
        // Both cannot-determine shapes that must NOT park — see the fn doc.
        RepoRefLookup::SnapshotRowMissing | RepoRefLookup::NotDerivable => {
            return proceed_past_gate(restore, api, name).await;
        }
        RepoRefLookup::ReferentMissing {
            kind,
            namespace: referent_ns,
            name: referent,
        } => {
            return park_on_missing_referent(
                ctx,
                restore,
                api,
                name,
                kind,
                referent_ns.as_deref(),
                &referent,
            )
            .await;
        }
    };
    match io::repository_ready_cached(ctx, &rref, &base_ns).await {
        Ok(true) => proceed_past_gate(restore, api, name).await,
        Ok(false) => {
            // patch-if-changed for byte-stability: the parked status is identical
            // across the 15s requeues, so a re-park is a server-side no-op — no
            // watch event, no self-trigger (the reconcile hot-loop rule).
            let current = serde_json::to_value(&restore.status).ok();
            // The repository object EXISTS, so any `ReferentAvailable=False` from
            // an earlier park is stale — clear it in this same patch rather than a
            // second conditions write (a merge patch replaces the whole array).
            let conditions = cleared_referent_conditions(restore)
                .unwrap_or_else(|| existing_conditions(restore));
            io::patch_status_if_changed(
                api,
                name,
                current.as_ref(),
                restore_ready_status_on(
                    restore,
                    &conditions,
                    RestorePhase::Pending,
                    crate::consts::REPOSITORY_NOT_READY_REASON,
                    &repository_not_ready_restore_message(&rref.name),
                ),
            )
            .await?;
            Ok(RepositoryGate::Held(Action::requeue(
                std::time::Duration::from_secs(15),
            )))
        }
        // The repository OBJECT is missing (not merely unreachable): the readiness
        // check answered nothing, so this is UNDETERMINED, not "proceed". Pre-#393
        // it fell through here and the wait window opened against an unverified
        // repository (#393).
        Err(Error::MissingDependency(_)) => {
            park_on_missing_referent(
                ctx,
                restore,
                api,
                name,
                io::repo_kind_str(rref.kind),
                repository_referent_namespace(&rref, &base_ns),
                &rref.name,
            )
            .await
        }
        Err(e) => Err(e),
    }
}

/// The namespace a missing repository referent was looked up in — `None` for a
/// `ClusterRepository`, which is cluster-scoped and whose message must not
/// invent one.
fn repository_referent_namespace<'a>(rref: &'a RepositoryRef, base_ns: &'a str) -> Option<&'a str> {
    match rref.kind {
        kopiur_api::common::RepositoryKind::Repository => {
            Some(rref.namespace.as_deref().unwrap_or(base_ns))
        }
        kopiur_api::common::RepositoryKind::ClusterRepository => None,
    }
}

/// Leave the gate open, clearing a stale
/// [`crate::consts::RESTORE_REFERENT_AVAILABLE_CONDITION`]
/// `= False` from an earlier park first.
///
/// The clear is guarded on the condition actually being present-and-not-True, so
/// the overwhelmingly common path (no such condition was ever written) costs
/// nothing and the healthy wire never grows the condition. Without it the
/// registry row — which is deliberately age-independent — would keep reporting a
/// restore that proceeded hours ago as blocked.
///
/// The cleared array is not just written to the server: the returned
/// [`RepositoryGate::Proceed`] carries a `Restore` holding it, which is the
/// object the rest of the pass must build on. The downstream gate parks rebuild
/// `conditions` from the `Restore` they are handed and patch it
/// UNCONDITIONALLY, so continuing from the reconcile-start copy would re-write
/// the `False` this just cleared — see [`restore_with_conditions`] for the write
/// loop that prevents. Nothing was cleared ⇒ the original is borrowed, no clone.
async fn proceed_past_gate<'a>(
    restore: &'a Restore,
    api: &Api<Restore>,
    name: &str,
) -> Result<RepositoryGate<'a>> {
    let cleared = cleared_referent_conditions(restore);
    if let Some(conditions) = &cleared {
        io::patch_status(api, name, serde_json::json!({ "conditions": conditions })).await?;
    }
    // One construction site, pure and total: a cleared pass CANNOT continue from
    // the stale object, because that combination is unwritable here.
    Ok(RepositoryGate::Proceed(carried_after_clear(
        restore,
        cleared.as_deref(),
    )))
}

/// Park a restore whose repository referent does not exist (issue #393):
/// `Pending` + the [`kopiur_api::gates::RESTORE_REFERENT_MISSING_GATE`] condition,
/// requeue 15s, and NO wait anchor — the caller returns before
/// [`ensure_wait_anchor`].
///
/// The Warning Event fires only on the park TRANSITION, gated by the same
/// status-changed check as the patch: the park re-patches a byte-identical
/// status every 15s (a server-side no-op), and Eventing that would be a
/// once-per-requeue spam. It compensates for what the pre-#393 fall-through
/// produced downstream — a `MissingDependency` error that `error_policy_for`
/// counted and Evented — which a clean park would otherwise silently remove.
async fn park_on_missing_referent(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    name: &str,
    kind: &str,
    referent_namespace: Option<&str>,
    referent: &str,
) -> Result<RepositoryGate<'static>> {
    let msg = referent_missing_restore_message(kind, referent_namespace, referent);
    let conditions = io::upsert_gate(
        &existing_conditions(restore),
        &kopiur_api::gates::RESTORE_REFERENT_MISSING_GATE,
        &msg,
        restore.metadata.generation,
    );
    let current = serde_json::to_value(&restore.status).ok();
    let changed = io::patch_status_if_changed(
        api,
        name,
        current.as_ref(),
        restore_ready_status_on(
            restore,
            &conditions,
            RestorePhase::Pending,
            crate::consts::RESTORE_REFERENT_MISSING_REASON,
            &msg,
        ),
    )
    .await?;
    if changed {
        io::publish_warning_event(
            ctx,
            restore,
            crate::consts::RESTORE_REFERENT_MISSING_REASON,
            crate::consts::CREATE_RESTORE_REFERENT_ACTION,
            &msg,
        )
        .await;
    }
    Ok(RepositoryGate::Undetermined(Action::requeue(
        std::time::Duration::from_secs(15),
    )))
}

/// Open the `waitTimeout` window if it is not open yet, and return where it stands for
/// THIS pass (#380).
///
/// The window is stamped ONCE, into `status.waitStartedAt`, on the first pass that gets
/// PAST [`gate_on_repository_readiness`] **and** — for a `target.populator` — has a PVC
/// claiming this Restore (`consumer`). "Past the gate" is what the code enforces, and it is
/// still slightly weaker than "the repository is Ready", but the gap is now deliberate and
/// narrow (#393). The gate no longer lets a MISSING referent through: a `Repository`
/// object that does not exist, or a `fromPolicy` `SnapshotPolicy` that does not exist,
/// parks as [`RepositoryGate::Undetermined`] and never reaches this function.
///
/// What still gets past the gate unverified, on purpose:
/// - a restore whose mover Job is already live or settled ([`restore_awaiting_launch`]) —
///   a live Job keeps the deadline it was dispatched with;
/// - a `snapshotRef` whose `Snapshot` ROW does not exist yet — that row is exactly what
///   the window is waiting for, and parking instead would prevent `onMissingSnapshot:
///   Fail` from ever firing for a typo'd ref;
/// - a referent that exists but derives no single repository (a spec bug, failed closed
///   downstream by `resolve_restore_repository`).
///
/// The ordering still guarantees the thing that matters, and now guarantees it for the
/// slow-arriving-referent case too: a restore parked waiting for its backend — or for the
/// object that names it — never opens a window.
///
/// Both halves of the condition are about a restore that cannot yet do anything measuring
/// a window it will need later:
/// - **Repository readiness.** A `Restore` applied by GitOps alongside its `Repository`
///   sits on the gate until the backend comes up. Anchored at creation, an outage longer
///   than `waitTimeout` means the first pass that reaches resolution already finds the
///   window closed and applies `onMissingSnapshot` — for `fromPolicy` that defaults to
///   `Continue`, i.e. an EMPTY volume.
/// - **A claiming PVC.** Resolution runs while a populator is `AwaitingClaim`, so a
///   standing populator `Restore` created months before anything claims it would burn its
///   whole window sitting idle and pin `Empty` the moment a claim finally appears.
///
/// Returns the *effective* window rather than relying on the caller re-reading the CR: on
/// the pass that opens it the stamp is not on the `restore` we were handed, and that is
/// precisely the pass that must not fall back to the creation timestamp.
/// [`WaitWindow::AwaitingClaim`] anchors at `now`, so the full window always remains — and
/// tells the parking caller to report the claim, not a phantom snapshot wait.
///
/// Writes NOTHING but `waitStartedAt`: the conditions array is replaced wholesale by a
/// merge patch, so a second conditions writer in one reconcile would erase the first.
///
/// No window configured ⇒ no anchor stamped: the value is only ever read through
/// `waitTimeout`, so a `Restore` without one carries no `status.waitStartedAt` at all.
async fn ensure_wait_anchor(
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    state: PopulatorState,
    has_claim: bool,
) -> Result<WaitWindow> {
    let now = chrono::Utc::now();
    let created = restore
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|t| t.0.as_second())
        .unwrap_or_else(|| now.timestamp());

    // Already open: honor the stamp verbatim (the window survives restarts).
    if restore
        .status
        .as_ref()
        .is_some_and(|s| s.wait_started_at.is_some())
    {
        return Ok(WaitWindow::Open(effective_wait_anchor(restore, created)));
    }
    // A populator with nothing claiming it cannot proceed, so its window has not opened.
    if !wait_window_opens(state, has_claim) {
        return Ok(WaitWindow::AwaitingClaim(now.timestamp()));
    }
    // No `waitTimeout` ⇒ nothing measures a window; don't stamp one.
    // `wait_remaining_secs`/`wait_deadline_rfc3339` both return `None` here.
    if restore
        .spec
        .policy
        .as_ref()
        .and_then(|p| p.wait_timeout.as_deref())
        .is_none()
    {
        return Ok(WaitWindow::Open(created));
    }
    let at = now.to_rfc3339();
    tracing::debug!(%namespace, restore = %name, wait_started_at = %at, "waitTimeout window opened");
    io::patch_status(api, name, serde_json::json!({ "waitStartedAt": at })).await?;
    Ok(WaitWindow::Open(now.timestamp()))
}

/// Map a resolved repository backend to the mover connect spec for a restore.
fn restore_connect(repo: &ResolvedRepository) -> Result<RepositoryConnect> {
    crate::snapshot::repository_connect_pub(repo)
}

/// The `Restore` CRD's kopia restore behavior knobs (ADR §4.6, M2 flag sweep),
/// carried through to the mover work-spec's `RestoreOp`.
struct RestoreFlags {
    ignore_permission_errors: Option<bool>,
    write_files_atomically: Option<bool>,
    parallel: Option<u32>,
    write_sparse_files: Option<bool>,
    skip_owners: Option<bool>,
    skip_permissions: Option<bool>,
    skip_times: Option<bool>,
    overwrite_files: Option<bool>,
    overwrite_directories: Option<bool>,
    overwrite_symlinks: Option<bool>,
    ignore_errors: Option<bool>,
    skip_existing: Option<bool>,
    delete_extra: bool,
}

/// Map `Restore.spec.options` onto the mover work-spec's restore flags. Pure so
/// it is unit-testable without a cluster — the regression guard for the M2 gap
/// sweep's bug class: plumbing that exists end-to-end (CRD field → workspec →
/// kopia client) but the controller never reads the field (`enableFileDeletion`
/// was exactly this: settable via CRD/CLI/migrate, consumed by nothing, so a
/// user's "exact mirror" restore was silently additive). An absent `options`
/// block maps every field to its all-`None`/`false` zero value, reproducing
/// today's argv exactly.
fn restore_flags(options: &Option<kopiur_api::restore::RestoreOptions>) -> RestoreFlags {
    let o = options.clone().unwrap_or_default();
    RestoreFlags {
        ignore_permission_errors: o.ignore_permission_errors,
        write_files_atomically: o.write_files_atomically,
        parallel: o.parallel,
        write_sparse_files: o.write_sparse_files,
        skip_owners: o.skip_owners,
        skip_permissions: o.skip_permissions,
        skip_times: o.skip_times,
        overwrite_files: o.overwrite_files,
        overwrite_directories: o.overwrite_directories,
        overwrite_symlinks: o.overwrite_symlinks,
        ignore_errors: o.ignore_errors,
        skip_existing: o.skip_existing,
        delete_extra: o.enable_file_deletion,
    }
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
