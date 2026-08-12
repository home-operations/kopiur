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
use kopiur_api::restore::ResolvedRestore;
use kopiur_api::snapshot::Snapshot;
use kopiur_api::{
    OnMissingSnapshot, ResolutionOutcome, Restore, RestorePhase, RestoreSource, RestoreTarget,
    validate,
};
use kopiur_mover::workspec::{
    MoverOptions, MoverWorkSpec, Operation, RepositoryConnect, ResolvedIdentity as MoverIdentity,
    RestoreOp, RestoreSelection, RestoreSelector, SnapshotAnchor, TargetRef,
};

use crate::config;
use crate::consts::{
    ALLOW_PRIVILEGED_MOVER_ACTION, API_VERSION, CREDENTIALS_AVAILABLE_CONDITION,
    CREDENTIALS_PROJECTED_REASON, INHERIT_FALLBACK_REASON, MATCH_WORKLOAD_SECURITY_CONTEXT_ACTION,
    MISSING_RECORDED_IDENTITY_REASON, MOVER_PERMITTED_CONDITION, ORPHANED_PRIME_REAPED_REASON,
    POPULATE_HIJACKED_REASON, PRIVILEGED_MOVER_NOT_PERMITTED_REASON, RECORDED_APPLIED_REASON,
    RECORDED_PINNED_NO_UID_REASON, RECREATE_CLAIM_TO_RESTORE_ACTION,
    RESTORE_SECURITY_CONTEXT_COMPATIBLE_CONDITION, RESTORE_TARGET_ALREADY_BOUND_REASON,
    SECURITY_CONTEXT_COMPATIBLE_REASON, SECURITY_CONTEXT_INHERITED_CONDITION,
    SET_EXPLICIT_MOVER_CONTEXT_ACTION,
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

/// The already-pinned resolution decision, or `None` when the source still has to be
/// resolved. Pure + exhaustive, so the data-safety invariant (ADR §4.6: "pinned once,
/// never re-resolved") lives in one testable place. Cases:
/// - `resolution: NoSnapshot` pinned → [`Resolution::Empty`] (a later snapshot never retargets).
/// - `kopiaSnapshotID` pinned (with or without the newer `resolution: Snapshot`, so a
///   legacy pin written before that field existed still reads correctly) → [`Resolution::Snapshot`].
/// - **No pin at all but the populator is already `Completed`**: the pre-fix buggy
///   `Continue` arm stamped `Completed` without pinning anything. A snapshot-resolved
///   populator ALWAYS pins before it can reach `Completed`, so `Completed` + unpinned ⟹ the
///   decision was "empty". Treat it as [`Resolution::Empty`] and re-pin it on the next write
///   — DO NOT re-resolve, or a snapshot that appeared after the original decision would
///   silently restore over the (possibly already-bound, in-use) volume.
///
///   `noop_already_bound` carves the ONE other way a populator reaches `Completed` while
///   unpinned out of that back-fill: an already-bound no-op (#233) on a DEFERRED source
///   never ran the mover, and the mover is what pins a deferred source. Back-filling
///   `NoSnapshot` there would durably record "this restore decided to come up empty", so a
///   later, legitimate recreation of the claiming PVC would provision an EMPTY volume
///   instead of restoring the snapshot. The three `Completed` populator states are told
///   apart by their `Ready` reason: legacy stuck (`PopulatingPrimePvc`, back-fill),
///   already-bound no-op (`TargetAlreadyBound`, re-resolve later), and a genuine
///   deploy-or-restore (`NoSnapshotContinue`, which pinned and so never reaches this arm).
/// - Otherwise → `None` (resolve now).
fn pinned_decision(
    resolved: Option<&ResolvedRestore>,
    phase: Option<&RestorePhase>,
    state: PopulatorState,
    noop_already_bound: bool,
) -> Option<Resolution> {
    match resolved {
        Some(r) if r.resolution == Some(ResolutionOutcome::NoSnapshot) => Some(Resolution::Empty),
        Some(r) => r.kopia_snapshot_id.clone().map(Resolution::Snapshot),
        None if phase == Some(&RestorePhase::Completed)
            && state == PopulatorState::AwaitingClaim
            && !noop_already_bound =>
        {
            Some(Resolution::Empty)
        }
        None => None,
    }
}

/// The work a populator pass has to do, once the source decision is known. Distinguishing
/// [`Self::EmptyVolume`] from [`Self::Unresolved`] is a data-safety requirement: both carry
/// "no snapshot selection", but the first means *deliberately provision an empty volume*
/// (deploy-or-restore) while the second means *we did not even look* — and provisioning an
/// empty volume for the second would lose data (#233).
enum PopulatorWork {
    /// Restore this selection (a controller-pinned id, or a selector the mover resolves).
    Restore(RestoreSelection),
    /// Deploy-or-restore: no snapshot matched, so the empty prime PVC IS the result.
    EmptyVolume,
    /// The source was deliberately NOT resolved this pass — the `Restore` already completed
    /// as an already-bound no-op, and re-resolving on every heartbeat would poll (and
    /// eventually error-loop on) a `SnapshotPolicy`/`Repository` the user has since deleted.
    /// If the claim turns out to need populating after all, the driver re-opens resolution
    /// rather than provisioning an empty volume.
    Unresolved,
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
    if let Some(action) =
        gate_on_repository_readiness(ctx, restore, &api, &namespace, &name).await?
    {
        return Ok(action);
    }

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
    // `pinned_decision` reads it (incl. legacy pins + the pre-fix stuck-populator
    // back-fill), returning `None` only when the source still has to be resolved.
    let resolved = restore.status.as_ref().and_then(|s| s.resolved.as_ref());
    let phase = restore.status.as_ref().and_then(|s| s.phase.as_ref());

    // A populator that completed as an already-bound NO-OP (#233) keeps reconciling
    // forever — its `Completed` is deliberately non-terminal precisely so a recreated
    // claim can still be populated later. Do NOT re-resolve its source on every one of
    // those heartbeats: for `fromPolicy` that is a SnapshotPolicy + Repository GET per
    // pass, and it turns into a `MissingDependency` error loop the moment the user deletes
    // either (likely — as far as they are concerned the restore is done) on a CR whose
    // status truthfully reads Completed/Ready. `drive_populator_restore` re-opens
    // resolution if the claim ever comes back unbound.
    let noop_already_bound = state == PopulatorState::AwaitingClaim
        && phase == Some(&RestorePhase::Completed)
        && completed_as_target_already_bound(restore);

    let decision = match pinned_decision(resolved, phase, state, noop_already_bound) {
        Some(d) => Some(d),
        // Deliberately unresolved this pass (see above).
        None if noop_already_bound => None,
        None => Some(match resolve_snapshot(ctx, restore, &namespace).await? {
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
                    "SourceResolved",
                    "the restore source resolved to a concrete kopia snapshot \
                     (pinned to status.resolved)",
                );
                status["resolved"] = resolved;
                io::patch_status(&api, &name, status).await?;
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
                // No snapshot matched. While the `waitTimeout` window (anchored at
                // the Restore's creation) is open, keep waiting instead of giving
                // up — `onMissingSnapshot` applies only once the window closes
                // (ADR §4.6 G7).
                let now = chrono::Utc::now().timestamp();
                // Anchored at creation — or, for a populator whose claim was re-created
                // after an already-bound no-op, at that re-open (`wait_window_anchor`).
                let created = wait_window_anchor(
                    restore,
                    restore
                        .metadata
                        .creation_timestamp
                        .as_ref()
                        .map(|t| t.0.as_second())
                        .unwrap_or(now),
                );
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
                match on_missing {
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
        }),
    };

    // Dispatch the decision to the target driver. Exhaustive over `Resolution` so a new
    // variant must be handled in both drivers before it compiles.
    match state {
        PopulatorState::DirectTarget => {
            // The mover work: a concrete id, an in-Job selector, or nothing (the
            // deploy-or-restore empty volume — the only case that skips the mover).
            let selection = match decision {
                Some(Resolution::Snapshot(id)) => Some(RestoreSelection::Snapshot(id)),
                Some(Resolution::Deferred(sel)) => Some(RestoreSelection::Resolve(sel)),
                Some(Resolution::Empty) => None,
                // Only a populator ever skips resolution (`noop_already_bound`).
                None => {
                    return Err(Error::Invariant(
                        "direct restore reached dispatch with an unresolved source".into(),
                    ));
                }
            };
            // A direct restore always acts on its decision, so pin an empty outcome here.
            if selection.is_none() {
                pin_no_snapshot(&api, restore, &name).await?;
            }
            drive_direct_restore(ctx, restore, &api, &namespace, &name, selection.as_ref()).await
        }
        PopulatorState::AwaitingClaim => {
            let work = match decision {
                Some(Resolution::Snapshot(id)) => {
                    PopulatorWork::Restore(RestoreSelection::Snapshot(id))
                }
                Some(Resolution::Deferred(sel)) => {
                    PopulatorWork::Restore(RestoreSelection::Resolve(sel))
                }
                Some(Resolution::Empty) => PopulatorWork::EmptyVolume,
                None => PopulatorWork::Unresolved,
            };
            drive_populator_restore(ctx, restore, &api, &namespace, &name, &work).await
        }
    }
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
                "Snapshot `{snapshot}` recorded no pinned uid — and no gid/fsGroup beyond \
                 the mover's own defaults — so inheriting it contributed nothing: the mover \
                 runs as its image's uid {MOVER_NONROOT_ID}, an identity that did NOT come \
                 from the backup ({provenance}). The backup mover's identity was \
                 image-determined at backup time. Pin mover.securityContext.runAsUser on \
                 this Restore if the restored files must be owned by a specific uid."
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
            " The mover runs as ROOT (uid 0) because Snapshot `{snapshot}` recorded uid 0 \
             — recorded metadata is repository data, forgeable by anyone with repository \
             write credentials, so verify this snapshot is trusted; the run is additionally \
             gated on the namespace's privileged-movers opt-in."
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
///
/// Where the handshake stands is a single exhaustive [`populator_handshake`] verdict over
/// the claiming PVC and the PV we rebound to it. That closed decision is what keeps #233
/// fixed: a populator can only hand a volume to an UNBOUND claim, so a `Restore` recreated
/// over an already-bound claim ([`PopulatorHandshake::NothingToPopulate`]) completes as a
/// truthful no-op and reaps any leftover prime instead of restoring into a prime that can
/// never be adopted.
///
/// `work` is [`PopulatorWork::EmptyVolume`] for deploy-or-restore (`onMissingSnapshot:
/// Continue` with no matching snapshot): the empty, freshly-provisioned prime PVC IS the
/// result, so the mover is skipped and the empty prime is rebound straight to the claiming
/// PVC. [`PopulatorWork::Restore`] runs the mover into the prime (a deferred selector
/// resolves in-Job, and may itself find no snapshot under `Continue` — then the prime stays
/// empty).
async fn drive_populator_restore(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    work: &PopulatorWork,
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

    // Steady-state exit for a Restore that has already settled over a BOUND claim (a
    // finished restore, or an already-bound no-op) and has no prime left to reap: nothing
    // observable can change until the claim itself is re-created, which re-enqueues us with
    // an UNBOUND claim and so cannot be missed. This keeps the per-heartbeat cost at one
    // namespaced GET instead of the unfiltered, cluster-wide PersistentVolume LIST below —
    // which, at the fleet scale this bug was reported from (one populator Restore per app),
    // is pure read amplification against a handshake that is over. Gated on the prime being
    // gone, so a reap whose best-effort delete did not land is still retried.
    let settled_over_bound_claim = restore.status.as_ref().and_then(|s| s.phase.as_ref())
        == Some(&RestorePhase::Completed)
        && kstatus_settled_for(restore, &RestorePhase::Completed)
        && pvc_is_bound(&consumer);
    if settled_over_bound_claim && pvc_api.get_opt(&prime_name).await?.is_none() {
        return Ok(Action::requeue(std::time::Duration::from_secs(600)));
    }

    // The PV our populator earmarked for this consumer: claimRef → THIS consumer (name +
    // uid) AND our rebind annotation. `Some` proves the prime→consumer rebind was already
    // issued, so we never recreate the prime past that point.
    let rebound = our_rebound_pv(ctx, namespace, &consumer_name, &consumer_uid).await?;

    // The one closed decision: is there anything to populate, and if not, why? Exhaustive
    // (no `_ =>`), so a new binding state cannot compile until it is handled here.
    let selection = match populator_handshake(&consumer, rebound.as_deref()) {
        // The handover landed: restore the PV's reclaim policy, GC the artifacts, complete.
        PopulatorHandshake::FinalizeRebound { pv } => {
            return finalize_populator_success(
                ctx,
                restore,
                api,
                namespace,
                name,
                &populate_job,
                &prime_name,
                &pv,
            )
            .await;
        }
        // Rebind issued; wait for the PV controller to bind our PV to the consumer.
        PopulatorHandshake::AwaitingBind => {
            return Ok(Action::requeue(std::time::Duration::from_secs(5)));
        }
        // #233: the claim is already bound, so there is nothing to populate. Complete as a
        // truthful no-op and reap any prime/Job that can never be handed over — including
        // the orphans ≤0.7.x left `Bound` forever holding a full copy of the restored data.
        PopulatorHandshake::NothingToPopulate => {
            return complete_populator_already_bound(
                ctx,
                restore,
                api,
                namespace,
                name,
                &populate_job,
                &prime_name,
                &consumer,
                PrimePvAction::Leave,
            )
            .await;
        }
        // Our rebind was issued but a different PV won the claim: the handover is lost.
        // Same no-op completion, but our PV holds the restored data — keep it (`Retain`).
        PopulatorHandshake::LostRebind { pv } => {
            return complete_populator_already_bound(
                ctx,
                restore,
                api,
                namespace,
                name,
                &populate_job,
                &prime_name,
                &consumer,
                PrimePvAction::ForceRetain(&pv),
            )
            .await;
        }
        // Unbound claim, no rebind outstanding: populate it.
        PopulatorHandshake::Populate => match work {
            PopulatorWork::Restore(sel) => Some(sel),
            PopulatorWork::EmptyVolume => {
                // We are about to provision the empty volume, so NOW the deploy-or-restore
                // decision is real and must be pinned (a later snapshot must never restore
                // over it). Pinning any earlier would also pin a no-op that provisioned
                // nothing — see `pin_no_snapshot`.
                pin_no_snapshot(api, restore, name).await?;
                None
            }
            // The claim is unbound but we deliberately skipped resolution this pass (this
            // Restore had completed as an already-bound no-op and the claim has since been
            // re-created). Re-open resolution rather than treating "no selection" as
            // deploy-or-restore, which would provision an EMPTY volume over a claim the
            // user expects restored.
            PopulatorWork::Unresolved => {
                return reopen_populator_resolution(api, restore, name, &consumer_name).await;
            }
        },
    };

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

    // Provision the prime PVC (mirrors the claim's spec, no dataSourceRef). For a real
    // restore, run the mover into it; for deploy-or-restore (no snapshot) the empty prime
    // IS the result — skip the mover and rebind it straight to the consumer.
    ensure_prime_pvc(
        ctx,
        restore,
        namespace,
        &prime_name,
        &consumer,
        selected_node.as_deref(),
    )
    .await?;

    if let Some(selection) = selection {
        match run_restore_mover(
            ctx,
            restore,
            api,
            namespace,
            &populate_job,
            &prime_name,
            selection,
            true, // the populate Job name is re-used across every claim this Restore populates
        )
        .await?
        {
            MoverOutcome::Running { created } => {
                let phase = restore.status.as_ref().and_then(|s| s.phase.as_ref());
                if created || phase != Some(&RestorePhase::Restoring) {
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
                let phase = restore.status.as_ref().and_then(|s| s.phase.as_ref());
                if phase != Some(&RestorePhase::Failed) {
                    io::patch_status(
                        api,
                        name,
                        restore_ready_status(
                            restore,
                            RestorePhase::Failed,
                            "MoverJobFailed",
                            "the populator restore mover Job failed; see the Job/pod logs, fix \
                             the cause, and re-create the claiming PVC — a Failed Restore is \
                             terminal",
                        ),
                    )
                    .await?;
                }
                return Ok(Action::requeue(std::time::Duration::from_secs(120)));
            }
            MoverOutcome::Wedged { message } => {
                let phase = restore.status.as_ref().and_then(|s| s.phase.as_ref());
                if phase != Some(&RestorePhase::Failed) {
                    io::patch_status(
                        api,
                        name,
                        restore_ready_status(
                            restore,
                            RestorePhase::Failed,
                            "MoverPodWedged",
                            &message,
                        ),
                    )
                    .await?;
                }
                return Ok(Action::requeue(std::time::Duration::from_secs(120)));
            }
            MoverOutcome::Succeeded { .. } => {}
        }
    } else {
        // Deploy-or-restore: no mover. Pin an observable in-flight phase once (so
        // `kubectl get restore` shows progress, not a stale `Pending`, while the empty
        // prime is rebound), then fall through to the rebind.
        let phase = restore.status.as_ref().and_then(|s| s.phase.as_ref());
        if phase != Some(&RestorePhase::Restoring) {
            io::patch_status(
                api,
                name,
                restore_ready_status(
                    restore,
                    RestorePhase::Restoring,
                    "NoSnapshotContinue",
                    "populator: no snapshot found; provisioning an empty volume \
                     (deploy-or-restore)",
                ),
            )
            .await?;
        }
    }

    // Mover done (or skipped for an empty volume): hand the prime PV to the consumer, then
    // requeue so the next pass observes the bind and finalizes. `rebind_prime_to_consumer`
    // returns `false` (requeue soon) while the prime PV hasn't appeared yet.
    if !rebind_prime_to_consumer(ctx, namespace, &prime_name, &consumer_name, &consumer_uid).await?
    {
        return Ok(Action::requeue(std::time::Duration::from_secs(5)));
    }
    Ok(Action::requeue(std::time::Duration::from_secs(5)))
}

/// Complete a populator whose prime PV the claiming PVC actually bound
/// ([`PopulatorHandshake::FinalizeRebound`]): restore the PV's original reclaim policy,
/// GC the populate artifacts, and stamp the outcome.
///
/// The completion message branches on the PINNED resolution, not on whether a selection
/// was dispatched: the controller pins a concrete id, while the mover pins a deferred
/// selector — which may itself have found nothing under `Continue`. A real restore stamps
/// `RestoreSucceeded`; deploy-or-restore stamps `NoSnapshotContinue` + a `Resolved=True`
/// domain condition, so an empty outcome is self-describing rather than claiming data was
/// written.
#[allow(clippy::too_many_arguments)]
async fn finalize_populator_success(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    populate_job: &str,
    prime_name: &str,
    pv_name: &str,
) -> Result<Action> {
    // Re-read `status.resolved` fresh: for a deferred restore the mover pins it in its own
    // terminal PATCH, which the cached `restore` (watch cache) may not yet reflect when a
    // PVC/PV bind event triggers this finalize reconcile — the direct path guards the same
    // race. Fall back to the cached value for the controller-pinned paths (which pin before
    // dispatch).
    let resolved = api
        .get_opt(name)
        .await?
        .and_then(|r| r.status)
        .and_then(|s| s.resolved)
        .or_else(|| restore.status.as_ref().and_then(|s| s.resolved.clone()));
    let restored =
        resolved.and_then(|r| r.resolution) == Some(kopiur_api::ResolutionOutcome::Snapshot);
    let status = if restored {
        restore_ready_status(
            restore,
            RestorePhase::Completed,
            crate::consts::RESTORE_POPULATED_REASON,
            "populator: restored the snapshot into the claiming PVC and rebound the volume",
        )
    } else {
        let msg = "populator: no snapshot found; provisioned an empty volume for the \
                   claiming PVC (deploy-or-restore)";
        let conditions = io::upsert_condition(
            &existing_conditions(restore),
            "Resolved",
            true,
            "NoSnapshotContinue",
            msg,
            restore.metadata.generation,
        );
        restore_ready_status_on(
            restore,
            &conditions,
            RestorePhase::Completed,
            "NoSnapshotContinue",
            msg,
        )
    };
    // Write the outcome BEFORE tearing the artifacts down. `finalize_populator` strips the
    // rebind annotation — the only evidence that the bound PV came from US — so a crash (or
    // a transient API error) between the strip and this patch would leave a restore that
    // genuinely ran looking, forever, like a claim we never touched: the next pass would see
    // no annotation plus a bound claim, read it as `NothingToPopulate`, and relabel it
    // `TargetAlreadyBound` ("no restore ran"), telling the user to delete the very PVC
    // holding their freshly restored data. Patching first makes the reverse order harmless:
    // the annotation still identifies our PV, so a crash simply replays this finalize.
    io::patch_status(api, name, status).await?;
    finalize_populator(
        ctx,
        namespace,
        populate_job,
        prime_name,
        PrimePvAction::RestoreOriginalPolicy(pv_name),
    )
    .await?;
    Ok(Action::requeue(std::time::Duration::from_secs(600)))
}

/// Complete a populator whose claiming PVC is already bound, so there is nothing to
/// populate (#233) — the CSI handover only applies to an UNBOUND claim.
///
/// Reaps any populate artifacts that can never be handed over (this is what collects the
/// orphaned `prime-*` PVCs ≤0.7.x left `Bound` forever, each holding a full copy of the
/// restored data; a still-running mover is cancelled with them, because its output can
/// never reach the claim), then stamps a truthful terminal status.
///
/// The status write is self-gated on the kstatus trio rather than the phase alone, which
/// does three jobs at once:
/// - a legacy stuck orphan (`Completed` + `Ready=False/PopulatingPrimePvc`, the frozen
///   state #233 reports) is NOT settled, so it heals exactly once; and
/// - a SUCCESSFULLY finalized populator (`Completed` + `Ready=True/RestoreSucceeded`)
///   reaches this same path on every steady heartbeat once `finalize_populator` has
///   stripped its rebind annotation — it IS settled, so its truthful success message is
///   never clobbered by the no-op message, and its artifacts are already gone so nothing
///   is reaped and no Event fires; and
/// - a freshly recreated `Restore` over a bound claim is not settled, so it completes.
#[allow(clippy::too_many_arguments)]
async fn complete_populator_already_bound(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    populate_job: &str,
    prime_name: &str,
    consumer: &k8s_openapi::api::core::v1::PersistentVolumeClaim,
    pv: PrimePvAction<'_>,
) -> Result<Action> {
    use k8s_openapi::api::batch::v1::Job;
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;

    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);

    // An object already being deleted is NOT an artifact to reap: re-issuing the delete
    // (and re-publishing the Event) on every heartbeat while it sits Terminating on a
    // finalizer would be pure churn.
    let live = |meta: &kube::core::ObjectMeta| meta.deletion_timestamp.is_none();
    let prime_live = pvc_api
        .get_opt(prime_name)
        .await?
        .is_some_and(|p| live(&p.metadata));
    let job = job_api
        .get_opt(populate_job)
        .await?
        .filter(|j| live(&j.metadata));
    let consumer_name = consumer.name_any();
    let bound_volume = consumer
        .spec
        .as_ref()
        .and_then(|s| s.volume_name.as_deref())
        .filter(|v| !v.is_empty());

    // A populate still RUNNING when the claim binds elsewhere is not a benign "nothing to
    // populate" — it is a handover hijacked mid-flight (see [`fail_populate_hijacked`]).
    if let Some(job) = job.as_ref()
        && crate::snapshot::job_terminal_state(job).is_none()
    {
        return fail_populate_hijacked(
            ctx,
            restore,
            api,
            namespace,
            name,
            populate_job,
            &consumer_name,
            bound_volume,
        )
        .await;
    }

    let kept_pv = match pv {
        PrimePvAction::ForceRetain(p) => Some(p),
        PrimePvAction::RestoreOriginalPolicy(_) | PrimePvAction::Leave => None,
    };
    let mut artifacts = Vec::new();
    if prime_live {
        artifacts.push(format!("prime PVC `{prime_name}`"));
    }
    if job.is_some() {
        artifacts.push(format!("populate Job `{populate_job}`"));
    }
    if !artifacts.is_empty() || kept_pv.is_some() {
        finalize_populator(ctx, namespace, populate_job, prime_name, pv).await?;
        let note = reaped_populate_artifacts_note(&artifacts, &consumer_name, kept_pv);
        tracing::warn!(%namespace, restore = %name, consumer = %consumer_name, "{note}");
        io::publish_warning_event(
            ctx,
            restore,
            ORPHANED_PRIME_REAPED_REASON,
            RECREATE_CLAIM_TO_RESTORE_ACTION,
            &note,
        )
        .await;
    }

    let settled = restore.status.as_ref().and_then(|s| s.phase.as_ref())
        == Some(&RestorePhase::Completed)
        && kstatus_settled_for(restore, &RestorePhase::Completed);
    if !settled {
        // A lost rebind gets its OWN message: a prime was provisioned, a restore DID run,
        // and a full-size volume is now retained — saying "nothing was provisioned, no
        // restore ran" there would hide storage the admin has just become responsible for.
        let msg = match kept_pv {
            Some(pv) => lost_rebind_message(&consumer_name, pv),
            None => target_already_bound_message(&consumer_name, bound_volume),
        };
        io::patch_status(
            api,
            name,
            restore_ready_status(
                restore,
                RestorePhase::Completed,
                RESTORE_TARGET_ALREADY_BOUND_REASON,
                &msg,
            ),
        )
        .await?;
    }
    Ok(Action::requeue(std::time::Duration::from_secs(600)))
}

/// The claiming PVC was bound out from under a populate that was STILL RUNNING: some
/// provisioner handed it a volume without honoring its `dataSourceRef`, so the restore we
/// are writing can never be delivered to it.
///
/// This is NOT the already-bound no-op (#233) even though it looks like one from the API:
/// there the claim was bound long before, by an earlier successful restore, and the app is
/// running on its own data. Here the app is about to come up on somebody else's — probably
/// empty — volume, so completing `Ready=True` would tell `kubectl wait`, Flux and Argo that
/// a restore landed when it did not. Fail honestly, cancel the doomed run, and LEAVE the
/// prime PVC in place: it holds the data we were mid-way through writing, and a `Failed`
/// populator is terminal at the reconcile guard, so nothing reaps it behind the user's back.
#[allow(clippy::too_many_arguments)]
async fn fail_populate_hijacked(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    populate_job: &str,
    consumer_name: &str,
    bound_volume: Option<&str>,
) -> Result<Action> {
    let msg = populate_hijacked_message(consumer_name, bound_volume);
    tracing::warn!(%namespace, restore = %name, consumer = %consumer_name, "{msg}");
    io::delete_mover_run(&ctx.client, namespace, populate_job).await?;
    io::publish_warning_event(
        ctx,
        restore,
        POPULATE_HIJACKED_REASON,
        RECREATE_CLAIM_TO_RESTORE_ACTION,
        &msg,
    )
    .await;
    if restore.status.as_ref().and_then(|s| s.phase.as_ref()) != Some(&RestorePhase::Failed) {
        io::patch_status(
            api,
            name,
            restore_ready_status(
                restore,
                RestorePhase::Failed,
                POPULATE_HIJACKED_REASON,
                &msg,
            ),
        )
        .await?;
    }
    Ok(Action::requeue(std::time::Duration::from_secs(600)))
}

/// Re-open source resolution on a populator that had completed as an already-bound no-op
/// and whose claiming PVC has since been re-created (and is unbound again, so it genuinely
/// wants populating).
///
/// Resolution is skipped on the no-op heartbeat (see `reconcile_inner`), so this pass has
/// no snapshot selection to act on — and MUST NOT read that absence as deploy-or-restore,
/// which would provision an empty volume over a claim the user expects restored. Moving
/// the phase off `Completed` (and the `Ready` reason off `TargetAlreadyBound`) makes the
/// next pass resolve normally; an already-pinned snapshot id is honored unchanged, so the
/// re-created claim gets the SAME snapshot (ADR §4.6: pinned once, never re-resolved).
async fn reopen_populator_resolution(
    api: &Api<Restore>,
    restore: &Restore,
    name: &str,
    consumer_name: &str,
) -> Result<Action> {
    let msg = format!(
        "populator: the claiming PVC `{consumer_name}` is unbound again (re-created), so this \
         Restore has work to do after all — re-resolving the restore source"
    );
    io::patch_status(
        api,
        name,
        restore_ready_status(
            restore,
            RestorePhase::Resolving,
            crate::consts::RESTORE_CLAIM_RECREATED_REASON,
            &msg,
        ),
    )
    .await?;
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

/// The `PersistentVolume` our populator earmarked for this consumer: its `claimRef` targets
/// the consumer by name AND uid, and it carries [`PRIME_ORIGINAL_RECLAIM_ANNOTATION`]
/// (stamped during the rebind). `Some` ⇒ the prime→consumer rebind has been issued.
///
/// The uid match is load-bearing: `claimRef` is pinned to a specific PVC instance, so a PV
/// left annotated from a PREVIOUS claim of the same name (deleted and re-created) must not
/// be mistaken for a rebind of the current one — the PV controller will never bind a
/// stale-uid `claimRef`, so treating it as an outstanding rebind would wedge the handshake
/// on a 5s requeue forever.
async fn our_rebound_pv(
    ctx: &Context,
    namespace: &str,
    consumer_name: &str,
    consumer_uid: &str,
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
                            && cr.uid.as_deref() == Some(consumer_uid)
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
                "NoSnapshotContinue",
                msg,
                restore.metadata.generation,
            );
            restore_ready_status_on(
                restore,
                &conditions,
                RestorePhase::Completed,
                "NoSnapshotContinue",
                msg,
            )
        }
        Some(kopiur_api::ResolutionOutcome::Snapshot) => restore_ready_status(
            restore,
            RestorePhase::Completed,
            "RestoreSucceeded",
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
                "NoSnapshotContinue",
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
                    "NoSnapshotContinue",
                    msg,
                ),
            )
            .await?;
        }
        return Ok(Action::requeue(std::time::Duration::from_secs(600)));
    };

    // The Job is named after the Restore and writes into the explicit target PVC;
    // the helper creates/tracks it, the phase writes stay here.
    match run_restore_mover(
        ctx,
        restore,
        api,
        namespace,
        name,
        &target_pvc,
        selection,
        false, // a direct restore is one-shot: its Job name is never re-used
    )
    .await?
    {
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
                "MoverJobFailed",
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

#[allow(clippy::too_many_arguments)]
async fn run_restore_mover(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    job_name: &str,
    target_pvc: &str,
    selection: &RestoreSelection,
    reusable_job_name: bool,
) -> Result<MoverOutcome> {
    use k8s_openapi::api::batch::v1::Job;
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    if let Some(job) = job_api.get_opt(job_name).await? {
        // A Job being DELETED is not THIS run's outcome — but only where the Job name is
        // re-used across runs, i.e. the populator (`<restore>-populate`, one name for every
        // claim the reusable Restore ever populates). There, a reaped-then-immediately-
        // re-populated claim could read the outgoing Job's terminal `Succeeded` as its own
        // and rebind a still-empty prime. Wait for the delete to land, then create a fresh Job.
        //
        // A DIRECT restore is one-shot, so its Job name is never re-used: a terminating Job
        // there IS this run's Job (typically TTL-reaped after succeeding), and ignoring its
        // terminal state would drop the outcome and re-run a full restore over a target PVC
        // the workload may already be writing to.
        if reusable_job_name && job.metadata.deletion_timestamp.is_some() {
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
    let recorded_source =
        match snapshot_recorded_source(ctx, restore, namespace, dispatch_pinned_id).await {
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
            let triple = crate::snapshot_policy::config_identity(
                &config,
                cfg_ns,
                repo.identity_defaults.as_ref(),
            )?;
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
) -> Result<Option<ResolveOutcome>> {
    use kopiur_api::common::{ObjectRef, ResolvedIdentity};
    // Selector policy is the same for both deferred arms: the per-mode default
    // onMissing unless the spec overrides it, and the waitTimeout window as an
    // ABSOLUTE deadline anchored at the Restore's creation (so the in-Job wait
    // matches the snapshotRef path and is stable across pod retries), polled by
    // the mover until that wall-clock instant.
    let on_missing = effective_on_missing(
        restore
            .spec
            .policy
            .as_ref()
            .and_then(|p| p.on_missing_snapshot),
        &restore.spec.source,
    );
    let wait_deadline = restore
        .spec
        .policy
        .as_ref()
        .and_then(|p| p.wait_timeout.as_deref())
        .and_then(crate::snapshot_schedule::parse_go_duration)
        .and_then(|d| {
            let created = restore.metadata.creation_timestamp.as_ref()?.0.as_second();
            let secs = i64::try_from(d.as_secs()).ok()?;
            chrono::DateTime::from_timestamp(created.checked_add(secs)?, 0).map(|t| t.to_rfc3339())
        });
    match &restore.spec.source {
        RestoreSource::SnapshotRef(r) => {
            let ns = r.namespace.as_deref().unwrap_or(namespace);
            let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), ns);
            let backup = api.get_opt(&r.name).await?;
            Ok(backup
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
                }))
        }
        RestoreSource::Identity(id) => {
            // An explicit snapshot id wins — pin it directly. Otherwise defer the
            // listing to the mover (works on every backend).
            if let Some(sid) = &id.snapshot_id {
                return Ok(Some(ResolveOutcome::Pinned(ResolvedSource {
                    kopia_snapshot_id: sid.clone(),
                    snapshot_ref: None,
                    identity: Some(ResolvedIdentity {
                        username: id.username.clone(),
                        hostname: id.hostname.clone(),
                        source_path: id.source_path.clone(),
                    }),
                })));
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
                .map(Some);
            }
            Ok(Some(ResolveOutcome::Deferred(RestoreSelector {
                username: id.username.clone(),
                hostname: id.hostname.clone(),
                source_path: id.source_path.clone(),
                as_of: id.as_of.clone(),
                offset: id.offset.unwrap_or(0),
                on_missing,
                wait_deadline: wait_deadline.clone(),
            })))
        }
        RestoreSource::FromPolicy(c) => {
            // Resolve the identity from the SnapshotPolicy, then defer the listing.
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
                .map(Some);
            }
            Ok(Some(ResolveOutcome::Deferred(RestoreSelector {
                username: identity.username,
                hostname: identity.hostname,
                source_path: identity.source_path,
                as_of: c.as_of.clone(),
                offset: c.offset,
                on_missing,
                wait_deadline: wait_deadline.clone(),
            })))
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
/// Returns `Ok(None)` whenever the ref cannot be determined: a missing
/// `snapshotRef`/`fromPolicy` referent, a Snapshot with neither pin nor repository
/// owner, or `identity` without the (required) explicit `spec.repository`.
/// Deliberately NON-FATAL — a `snapshotRef` whose Snapshot CR does not exist yet is
/// a supported shape (`resolve_snapshot` parks it on the `waitTimeout` window), so
/// erroring here would break it; the other absences produce their canonical
/// `MissingDependency`/`Validation` surfaces downstream. The gate simply does not
/// engage until the repository is known.
async fn restore_repository_ref(
    ctx: &Context,
    restore: &Restore,
    namespace: &str,
) -> Result<Option<(RepositoryRef, String)>> {
    if let Some(rref) = &restore.spec.repository {
        return Ok(Some((rref.clone(), namespace.to_string())));
    }
    match &restore.spec.source {
        RestoreSource::SnapshotRef(sref) => {
            // Resolved relative to the SNAPSHOT's namespace (an absent ref
            // namespace means "same as the snapshot", not "same as the restore")
            // — the same base `resolve_restore_repository` uses.
            let snap_ns = sref.namespace.as_deref().unwrap_or(namespace);
            let snap_api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), snap_ns);
            Ok(snap_api
                .get_opt(&sref.name)
                .await?
                .as_ref()
                .and_then(repository_ref_from_snapshot)
                .map(|rref| (rref, snap_ns.to_string())))
        }
        RestoreSource::FromPolicy(c) => {
            use kopiur_api::SnapshotPolicy;
            let cfg_ns = c.namespace.as_deref().unwrap_or(namespace);
            let cfg_api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), cfg_ns);
            Ok(cfg_api.get_opt(&c.name).await?.and_then(|cfg| {
                // Multi-repo fromPolicy with no explicit selection: the
                // readiness gate cannot know which repository to wait on, and
                // this contract is deliberately NON-FATAL — return None (never
                // guess repository #1); `resolve_restore_repository` fails
                // closed downstream with the valid choices listed.
                kopiur_api::single_repository_ref(&cfg.spec)
                    .ok()
                    .map(|r| (r.clone(), cfg_ns.to_string()))
            }))
        }
        RestoreSource::Identity(_) => Ok(None),
    }
}

/// The Restore peer of the Snapshot reconciler's repository-readiness gate: hold a
/// not-yet-launched restore in `Pending` (`Ready=False`/`RepositoryNotReady`,
/// non-terminal) while its repository is not `Ready`, and requeue on the same 15s
/// cadence. Returns `Some(requeue)` while gated, `None` to proceed.
///
/// Two semantic notes, both deliberate:
/// - **`spec.mode: ReadOnly` is a different axis.** The Snapshot reconciler's mode
///   gate refuses backups on a read-only repository and explicitly says "Restores
///   remain allowed (the Restore reconciler does not gate on mode)" — that stays
///   true; a read-only repo serves restores. Readiness (`status.phase == Ready`)
///   is REACHABILITY: an unreachable backend fails restores exactly like backups,
///   hence this gate.
/// - **`waitTimeout` interaction.** The wait window is anchored at the Restore's
///   creation as an ABSOLUTE deadline (see `resolve_snapshot` — spec'd so the
///   in-Job wait is stable across pod retries), so wall-clock time parked on this
///   gate DOES consume it. What running BEFORE the resolution machinery guarantees
///   is that the window is only ever EVALUATED against a Ready repository: an
///   outage can at worst make the post-recovery pass apply `onMissingSnapshot`
///   immediately (window already elapsed); it can never fail the restore — or fake
///   a `Continue` empty-volume outcome — while the repository is down.
///
/// Only a restore that has NOT launched its mover Job is gated
/// ([`restore_awaiting_launch`]); a `Restoring` restore's live Job is tracked to
/// terminal, never re-gated, mirroring the Snapshot reconciler's ordering. (Narrow
/// crash window: a Job created moments before the controller died — before the
/// `Restoring` patch landed — is re-gated as `Pending`/`Resolving`; harmless, the
/// Job runs to terminal on its own and is observed once the gate opens.)
async fn gate_on_repository_readiness(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
) -> Result<Option<Action>> {
    if !restore_awaiting_launch(restore.status.as_ref().and_then(|s| s.phase.as_ref())) {
        return Ok(None);
    }
    let Some((rref, base_ns)) = restore_repository_ref(ctx, restore, namespace).await? else {
        return Ok(None);
    };
    match io::repository_ready_cached(ctx, &rref, &base_ns).await {
        Ok(true) => Ok(None),
        Ok(false) => {
            // patch-if-changed for byte-stability: the parked status is identical
            // across the 15s requeues, so a re-park is a server-side no-op — no
            // watch event, no self-trigger (the reconcile hot-loop rule).
            let current = serde_json::to_value(&restore.status).ok();
            io::patch_status_if_changed(
                api,
                name,
                current.as_ref(),
                restore_ready_status(
                    restore,
                    RestorePhase::Pending,
                    crate::consts::REPOSITORY_NOT_READY_REASON,
                    &repository_not_ready_restore_message(&rref.name),
                ),
            )
            .await?;
            Ok(Some(Action::requeue(std::time::Duration::from_secs(15))))
        }
        // The repository OBJECT is missing (not merely unreachable): not this
        // gate's call — fall through so the resolve/dispatch paths surface their
        // canonical `MissingDependency` hold, exactly as before the gate existed.
        Err(Error::MissingDependency(_)) => Ok(None),
        Err(e) => Err(e),
    }
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
