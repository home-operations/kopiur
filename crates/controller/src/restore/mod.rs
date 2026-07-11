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
    CREDENTIALS_PROJECTED_REASON, MISSING_CREDENTIALS_REASON, MOVER_PERMITTED_CONDITION,
    PRIVILEGED_MOVER_NOT_PERMITTED_REASON, RESTORE_SECURITY_CONTEXT_COMPATIBLE_CONDITION,
    SECURITY_CONTEXT_COMPATIBLE_REASON,
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
/// - Otherwise → `None` (resolve now).
fn pinned_decision(
    resolved: Option<&ResolvedRestore>,
    phase: Option<RestorePhase>,
    state: PopulatorState,
) -> Option<Resolution> {
    match resolved {
        Some(r) if r.resolution == Some(ResolutionOutcome::NoSnapshot) => Some(Resolution::Empty),
        Some(r) => r.kopia_snapshot_id.clone().map(Resolution::Snapshot),
        None if phase == Some(RestorePhase::Completed)
            && state == PopulatorState::AwaitingClaim =>
        {
            Some(Resolution::Empty)
        }
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
    // here. Restore *duration* is still recorded at the Job-completion site.
    result
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
    match restore.status.as_ref().and_then(|s| s.phase) {
        Some(phase) if phase_is_terminal_at_guard(phase, state) => {
            return steady_terminal_restore(restore, &api, &name, phase).await;
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

    // ADR §4.6: the resolution is pinned ONCE and never re-resolved — a restore must
    // not silently retarget when newer snapshots appear mid-flight. The pinned decision
    // is a snapshot id OR a deliberate "no snapshot, deploy-or-restore" (`Resolution`);
    // `pinned_decision` reads it (incl. legacy pins + the pre-fix stuck-populator
    // back-fill), returning `None` only when the source still has to be resolved.
    let resolved = restore.status.as_ref().and_then(|s| s.resolved.as_ref());
    let phase = restore.status.as_ref().and_then(|s| s.phase);
    let decision = match pinned_decision(resolved, phase, state) {
        Some(d) => d,
        None => match resolve_snapshot(ctx, restore, &namespace).await? {
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
        },
    };

    // Pin the deploy-or-restore "no snapshot" decision exactly once. This durably records
    // the choice (so a snapshot that appears later can never silently restore over the
    // provisioned volume) AND back-fills the pre-fix stuck-populator state that
    // `pinned_decision` inferred from a `Completed`+unpinned Restore. Idempotent: re-pinning
    // an already-`NoSnapshot` resolution is a server-side no-op. A bare `{resolved}` merge
    // patch leaves phase/conditions to the target driver below (the `Resolved` domain
    // condition is stamped at the driver's empty completion).
    if decision == Resolution::Empty
        && resolved.and_then(|r| r.resolution) != Some(ResolutionOutcome::NoSnapshot)
    {
        let mut pin = serde_json::json!({ "pinnedAt": chrono::Utc::now().to_rfc3339() });
        pin["resolution"] = serde_json::to_value(ResolutionOutcome::NoSnapshot)?;
        io::patch_status(&api, &name, serde_json::json!({ "resolved": pin })).await?;
    }

    // The mover work the decision dispatches: a concrete id, an in-Job selector, or
    // nothing (deploy-or-restore empty volume). `None` is the only case that skips
    // the mover entirely. Exhaustive so a new `Resolution` variant must be handled.
    let selection = match &decision {
        Resolution::Snapshot(id) => Some(RestoreSelection::Snapshot(id.clone())),
        Resolution::Deferred(sel) => Some(RestoreSelection::Resolve(sel.clone())),
        Resolution::Empty => None,
    };

    match state {
        PopulatorState::DirectTarget => {
            drive_direct_restore(ctx, restore, &api, &namespace, &name, selection.as_ref()).await
        }
        PopulatorState::AwaitingClaim => {
            drive_populator_restore(ctx, restore, &api, &namespace, &name, selection.as_ref()).await
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
///
/// `selection` is `None` for deploy-or-restore (`onMissingSnapshot: Continue` with no
/// matching snapshot): the empty, freshly-provisioned prime PVC IS the result, so the
/// mover is skipped and the empty prime is rebound straight to the claiming PVC. A
/// `Some` selection runs the mover into the prime (a deferred selector resolves in-Job,
/// and may itself find no snapshot under `Continue` — then the prime stays empty).
async fn drive_populator_restore(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    name: &str,
    selection: Option<&RestoreSelection>,
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
            // Branch the completion on whether a snapshot was actually restored. Read the
            // pinned outcome (the controller pins it for a concrete id; the mover pins it
            // for a deferred selector, which may itself have found nothing under
            // `Continue`) rather than `selection.is_some()`, so a deferred no-match also
            // stamps the empty message. A restore stamps `RestoreSucceeded`;
            // deploy-or-restore (no snapshot) stamps `NoSnapshotContinue` + a
            // `Resolved=True` domain condition so the empty outcome is self-describing.
            // Re-read status.resolved fresh: for a deferred restore the mover pins it in
            // its own terminal PATCH, which the cached `restore` (watch cache) may not yet
            // reflect when a PVC/PV bind event triggers this finalize reconcile — the
            // direct path guards the same race. Fall back to the cached value for the
            // controller-pinned paths (which pin before dispatch).
            let resolved = api
                .get_opt(name)
                .await?
                .and_then(|r| r.status)
                .and_then(|s| s.resolved)
                .or_else(|| restore.status.as_ref().and_then(|s| s.resolved.clone()));
            let restored = resolved.and_then(|r| r.resolution)
                == Some(kopiur_api::ResolutionOutcome::Snapshot);
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
            io::patch_status(api, name, status).await?;
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
                let phase = restore.status.as_ref().and_then(|s| s.phase);
                if phase != Some(RestorePhase::Failed) {
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
        let phase = restore.status.as_ref().and_then(|s| s.phase);
        if phase != Some(RestorePhase::Restoring) {
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

    let phase = restore.status.as_ref().and_then(|s| s.phase);

    // Deploy-or-restore: no snapshot to write. The target PVC is ensured above (so a
    // `target.pvc` comes up empty rather than missing); a `pvcRef` already exists. Stamp
    // a terminal `Completed` via `restore_ready_status_on` (Ready=True) so the entry-guard
    // heal does NOT clobber this message with "the snapshot data was written" — there is no
    // mover here, so the controller is the sole writer (no two-writer race).
    let Some(selection) = selection else {
        if phase != Some(RestorePhase::Completed) {
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
    match run_restore_mover(ctx, restore, api, namespace, name, &target_pvc, selection).await? {
        MoverOutcome::Succeeded { duration_secs } => {
            if let Some(secs) = duration_secs {
                ctx.metrics.set_restore_duration(namespace, name, secs);
            }
            if phase != Some(RestorePhase::Completed) {
                // A deferred (object-store/identity) resolution may have come up
                // empty under `Continue`. The mover pins the outcome to
                // status.resolved before its Job goes terminal, so a fresh read
                // tells a real restore from a deploy-or-restore-empty — without it
                // the success message would falsely claim "data was written".
                let resolved = api
                    .get_opt(name)
                    .await?
                    .and_then(|r| r.status)
                    .and_then(|s| s.resolved);
                io::patch_status(
                    api,
                    name,
                    restore_success_status(restore, resolved.as_ref()),
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
    phase: RestorePhase,
) -> Result<Action> {
    if !kstatus_settled_for(restore, phase) {
        let status = if phase == RestorePhase::Completed {
            // The mover wrote `phase: Completed` + `status.resolved` in one
            // PATCH, so `resolved` is observed here — distinguish a real
            // restore from a deploy-or-restore that came up empty.
            restore_success_status(
                restore,
                restore.status.as_ref().and_then(|s| s.resolved.as_ref()),
            )
        } else {
            restore_ready_status(
                restore,
                phase,
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

async fn run_restore_mover(
    ctx: &Context,
    restore: &Restore,
    api: &Api<Restore>,
    namespace: &str,
    job_name: &str,
    target_pvc: &str,
    selection: &RestoreSelection,
) -> Result<MoverOutcome> {
    use k8s_openapi::api::batch::v1::Job;
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    if let Some(job) = job_api.get_opt(job_name).await? {
        return observe_restore_mover(ctx, restore, namespace, job_name, &job).await;
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
/// time) survive the rewrite.
///
/// `snapshotRef` reads the referenced `Snapshot` CR's recorded status (which
/// keeps the correct identity/timing even when its id went stale). `identity`/
/// `fromPolicy` use the pinned `status.resolved.identity` source path
/// (best-effort, no start time — those resolve from a live list at admission).
/// Empty when nothing is recorded yet (the mover then restores by id only).
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
            return SnapshotAnchor {
                source_path: st
                    .and_then(|s| s.snapshot.as_ref())
                    .and_then(|s| s.identity.source_path.clone())
                    .unwrap_or_default(),
                start_time: st
                    .and_then(|s| s.timing.as_ref())
                    .and_then(|t| t.start_time.clone()),
            };
        }
    }
    let source_path = restore
        .status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .and_then(|r| r.identity.as_ref())
        .and_then(|i| i.source_path.clone())
        .or_else(|| match &restore.spec.source {
            RestoreSource::Identity(id) => id.source_path.clone(),
            _ => None,
        })
        .unwrap_or_default();
    SnapshotAnchor {
        source_path,
        start_time: None,
    }
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
/// works for filesystem repos, and the mover reaches every backend. Returns
/// `None` ONLY for a `snapshotRef` whose `Snapshot` CR has no resolved snapshot
/// yet (the caller applies `waitTimeout` + `onMissingSnapshot`); deferred sources
/// never return `None` — the mover applies `onMissingSnapshot` in-Job.
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
