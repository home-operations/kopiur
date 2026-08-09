//! The `SnapshotReplication` reconciler (issue #368) — logical (snapshot-level)
//! replication between two repository CRs via `kopia snapshot migrate`.
//!
//! Mirrors the `RepositoryReplication` scheduler (`crate::repository_replication`):
//! the controller is the *scheduler*. Each reconcile it decides whether a
//! replication is due (croner + deterministic jitter via
//! [`crate::snapshot_schedule::next_fire`], seeded by the CR UID), gates on BOTH
//! repositories being Ready (source and destination are real repository CRs
//! here, unlike blob replication's inline destination backend), on the
//! destination being writable, and on the runtime identity-overlap check, then
//! spawns at most one per-slot owned mover Job and tracks it to terminal. The
//! mover PATCHes `.status` (phase, `lastReplicated`, `lastRun`) and reconciles
//! the dest-side copy `Snapshot` CRs.
//!
//! Hardening matches replication/maintenance: per-slot deterministic Job names,
//! single-flight via a label selector, a requeue cap, and transition-guarded
//! status. Two extras live in the idle (not-due) arm: the cross-namespace
//! discovered-duplicate reap for `ClusterRepository` destinations (plan A6 —
//! the namespaced mover cannot delete a duplicate that materialized in another
//! namespace), and the projected-credentials reap (no live copy of either
//! repository's Secrets lingers between runs).

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use k8s_openapi::api::batch::v1::Job;
use kube::api::{DeleteParams, ListParams};
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};

use kopiur_api::common::{Encryption, RepositoryKind, RepositoryRef};
use kopiur_api::snapshot::{Origin, Snapshot};
use kopiur_api::snapshot_replication::{IdentityMatcher, PolicyCopyMode, Pruning};
use kopiur_api::{SnapshotPolicy, SnapshotReplication, SnapshotReplicationPhase, validate};
use kopiur_mover::workspec::{
    IdentityMatcherSpec, MirrorSourcePruningSpec, MoverOptions, MoverWorkSpec, NoPruningSpec,
    Operation, PolicyCopyModeSpec, PruningSpec, ReplicationRepositoryRef, ReplicationRetentionSpec,
    ReplicationSourceRef, ResolvedIdentity, SnapshotReplicateOp, TargetRef,
};

use crate::consts::{
    API_VERSION, COMPONENT_LABEL, ORIGIN_LABEL, REPOSITORY_UID_LABEL, SNAPSHOT_ID_LABEL,
    SNAPSHOT_REPLICATION_COMPONENT, SNAPSHOT_REPLICATION_INSTANCE_LABEL,
    SNAPSHOT_REPLICATION_SLOT_ANNOTATION,
};
use crate::context::Context;
use crate::error::{Error, Result, error_policy_for};
use crate::io::{self, ResolvedRepository};
use crate::jobs::{self, JobLimits, MoverJobInputs, VolumeMountSpec};
use crate::naming::short_hash;
use crate::snapshot::{backend_to_repository_connect, job_terminal_state};
use crate::snapshot_schedule::{next_fire, parse_go_duration};

/// How long a finished snapshot-replication Job lingers before TTL-reaping.
const SNAPSHOT_REPLICATION_JOB_TTL_SECS: i64 = 3600;
/// Requeue while a replication Job is in flight.
const REQUEUE_RUNNING: Duration = Duration::from_secs(30);
/// Requeue while waiting for a repository to become Ready (or a gate to clear).
const REQUEUE_NOT_READY: Duration = Duration::from_secs(60);
/// Requeue after a failed replication Job (re-check / bounded retry once TTL-reaped).
const REQUEUE_FAILED: Duration = Duration::from_secs(300);
/// Upper bound on any requeue so the schedule/readiness is re-evaluated.
const REQUEUE_CAP: Duration = Duration::from_secs(1800);

/// Condition reporting the runtime identity-overlap check: `True` when a
/// destination-side `SnapshotPolicy` writes directly under an identity this
/// replication's selection would also copy into (replicated copies and direct
/// backups would interleave in one kopia identity's history). Warn-only unless
/// `pruning: mirrorSource` — that combination is the data-loss hazard the
/// reconcile STALLS on instead ([`REPLICATION_IDENTITY_OVERLAP_REASON`]).
const IDENTITY_OVERLAP_CONDITION: &str = "IdentityOverlap";
/// `reason` for [`IDENTITY_OVERLAP_CONDITION`] = `True`.
const IDENTITY_OVERLAP_REASON: &str = "DestinationPolicyIdentityOverlap";
/// `reason` for [`IDENTITY_OVERLAP_CONDITION`] = `False` (the consistent state).
const NO_IDENTITY_OVERLAP_REASON: &str = "NoOverlap";
/// `Ready`-stalled reason when `pruning: mirrorSource` meets an overlap — the
/// run is SKIPPED until the selection or the destination policy changes.
const REPLICATION_IDENTITY_OVERLAP_REASON: &str = "ReplicationIdentityOverlap";

/// Reconcile a `SnapshotReplication`.
#[tracing::instrument(skip(repl, ctx), fields(kind = "SnapshotReplication", namespace = %repl.namespace().unwrap_or_default(), name = %repl.name_any()))]
pub async fn reconcile(
    repl: std::sync::Arc<SnapshotReplication>,
    ctx: std::sync::Arc<Context>,
) -> Result<Action> {
    let start = std::time::Instant::now();
    let result = reconcile_inner(&repl, &ctx).await;
    ctx.metrics
        .record_reconcile("SnapshotReplication", start.elapsed().as_secs_f64());
    result
}

async fn reconcile_inner(repl: &SnapshotReplication, ctx: &Context) -> Result<Action> {
    // Defensive re-validation (one validator, two callers).
    let errs = validate::validate_snapshot_replication(&repl.spec);
    if let Some(first) = errs.into_iter().next() {
        return Err(Error::Validation(first.to_string()));
    }

    let namespace = repl
        .namespace()
        .ok_or_else(|| Error::Invariant("SnapshotReplication has no namespace".into()))?;
    let name = repl.name_any();
    let api: Api<SnapshotReplication> = Api::namespaced(ctx.client.clone(), &namespace);

    // Version skew: a phase written by a NEWER kopiur. Two facts shape what this
    // warning is for, and neither is the prompt-overwrite story the other
    // drivers have (see `io::warn_unreadable_phase`):
    //
    // - Nothing below READS `status.phase` — no branch, no gate, no dedupe — so
    //   an unreadable value changes no behavior: the replication keeps running
    //   its schedule normally.
    // - Nothing below promptly rewrites it either. Only the suspended and
    //   Job-failed paths pass a phase at all (and both go through
    //   `patch_ready_if_changed`, which short-circuits when nothing it writes
    //   changed); the waiting/idle/in-flight paths pass `phase: None` or do not
    //   patch, and the terminal `Succeeded`/`Failed` stamp comes from the mover
    //   at the END of a run. So the value PERSISTS — potentially a whole
    //   schedule interval, until the next run stamps over it.
    //
    // Which is exactly why the warn is unconditional and repeats on every pass
    // (every 30s while a Job is in flight, otherwise up to the requeue cap): the
    // log is the only place this skew surfaces, so it has to keep surfacing
    // until the operator upgrade finishes.
    if let Some(label) = unreadable_phase(repl) {
        io::warn_unreadable_phase("SnapshotReplication", &namespace, &name, label);
    }

    // A suspended replication is skipped (surface phase + Ready=Reconciling).
    if repl.spec.suspend {
        patch_ready_if_changed(
            &api,
            &name,
            repl,
            io::ReadyOutcome::Reconciling,
            "Suspended",
            "replication is suspended (spec.suspend)",
            Some(SnapshotReplicationPhase::Suspended),
            None,
        )
        .await?;
        return Ok(Action::requeue(REQUEUE_CAP));
    }

    // Both endpoints are real repository CRs — resolve them (backend, encryption,
    // moverDefaults, CA bundles), source first for message determinism.
    let source = io::resolve_repository_ref(
        &ctx.client,
        &repl.spec.source_ref,
        &namespace,
        ctx.operator_namespace.as_deref(),
    )
    .await?;
    let dest = io::resolve_repository_ref(
        &ctx.client,
        &repl.spec.destination_ref,
        &namespace,
        ctx.operator_namespace.as_deref(),
    )
    .await?;

    // Gate on BOTH repositories being Ready — an object-store repo must be
    // bootstrapped before `kopia snapshot migrate` can reach it. DISTINCT
    // reasons so `kubectl describe` names which end is holding the run.
    if !io::repository_ready(&ctx.client, &repl.spec.source_ref, &namespace).await? {
        patch_ready_if_changed(
            &api,
            &name,
            repl,
            io::ReadyOutcome::Reconciling,
            "WaitingForSourceRepository",
            "source repository is not Ready; deferring replication",
            None,
            None,
        )
        .await?;
        return Ok(Action::requeue(REQUEUE_NOT_READY));
    }
    if !io::repository_ready(&ctx.client, &repl.spec.destination_ref, &namespace).await? {
        patch_ready_if_changed(
            &api,
            &name,
            repl,
            io::ReadyOutcome::Reconciling,
            "WaitingForDestinationRepository",
            "destination repository is not Ready; deferring replication",
            None,
            None,
        )
        .await?;
        return Ok(Action::requeue(REQUEUE_NOT_READY));
    }

    // A read-only destination can never take migrated snapshots: stalled until
    // the destination's `spec.mode` changes (spec-shaped, not transient).
    if !dest.mode.allows_writes() {
        patch_ready_if_changed(
            &api,
            &name,
            repl,
            io::ReadyOutcome::Stalled,
            "DestinationReadOnly",
            "destination repository is mode: ReadOnly and cannot take replicated \
             snapshots; set its spec.mode to ReadWrite or point destinationRef elsewhere",
            None,
            None,
        )
        .await?;
        return Ok(Action::requeue(REQUEUE_NOT_READY));
    }

    // Runtime identity-overlap check (the webhook's admission-time sibling can
    // be raced by later policy edits — this is the standing backstop).
    let overlap = destination_identity_overlap(ctx, repl, &namespace).await?;
    if let OverlapVerdict::Stall { identities } = &overlap {
        patch_ready_if_changed(
            &api,
            &name,
            repl,
            io::ReadyOutcome::Stalled,
            REPLICATION_IDENTITY_OVERLAP_REASON,
            &format!(
                "pruning: mirrorSource with a destination-side SnapshotPolicy writing the \
                 same identities this replication selects ({}) — a source-side deletion \
                 would cascade into identities the destination does not merely mirror. \
                 Exclude those identities in spec.selection, re-identify the destination \
                 policy, or switch pruning off mirrorSource",
                identity_sample(identities)
            ),
            None,
            Some(&overlap),
        )
        .await?;
        return Ok(Action::requeue(REQUEUE_NOT_READY));
    }

    drive_schedule(ctx, repl, &api, &namespace, &name, &source, &dest, &overlap).await
}

/// The scheduling half of the reconcile: decide the due slot, drive the
/// per-slot Job to terminal, run the idle-arm heals. Split from
/// [`reconcile_inner`] so neither half trips the cognitive-complexity ratchet.
#[allow(clippy::too_many_arguments)]
async fn drive_schedule(
    ctx: &Context,
    repl: &SnapshotReplication,
    api: &Api<SnapshotReplication>,
    namespace: &str,
    name: &str,
    source: &ResolvedRepository,
    dest: &ResolvedRepository,
    overlap: &OverlapVerdict,
) -> Result<Action> {
    let now = Utc::now();
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    // Timezone precedence: `spec.schedule.timezone` wins; else the SOURCE
    // repository's `scheduleDefaults.timezone`; else UTC (GitHub #174 item 3 —
    // the source is where the data lives, so its operational zone anchors the
    // wall-clock schedule).
    let repo_tz = source
        .schedule_defaults
        .as_ref()
        .and_then(|d| d.timezone.as_deref());

    let Some(slot) = due_slot(repl, now, repo_tz) else {
        // The idle heal pass: reap cross-namespace discovered duplicates
        // (ClusterRepository destinations, A6) and any lingering projected
        // credential copies, then mark Ready (this is also where the two-pass
        // terminal heal lands: the mover stamped phase/lastReplicated, and this
        // arm heals Ready + observedGeneration + the overlap condition).
        idle_heal(ctx, repl, &job_api, namespace, name, dest).await?;
        patch_ready_if_changed(
            api,
            name,
            repl,
            io::ReadyOutcome::Ready,
            "Idle",
            "replication is reconciled; waiting for the next scheduled slot",
            None,
            Some(overlap),
        )
        .await?;
        return Ok(Action::requeue(cap(next_wakeup(repl, now, None, repo_tz))));
    };

    let job_name = snapshot_replication_job_name(name, slot);
    match job_api.get_opt(&job_name).await? {
        Some(job) => match job_terminal_state(&job) {
            // Success: the mover stamped status (phase/lastReplicated/lastRun);
            // requeue to the next slot ONLY — Ready/observedGeneration heal on
            // the next pass's not-due "Idle" arm (the two-pass contract: a
            // patch here would race the mover's own terminal write).
            Some(true) => Ok(Action::requeue(cap(next_wakeup(
                repl,
                now,
                Some(slot),
                repo_tz,
            )))),
            // Failure: the failed Job lingers to its TTL as the bounded-retry
            // backoff (and keeps the pod logs).
            Some(false) => {
                patch_ready_if_changed(
                    api,
                    name,
                    repl,
                    io::ReadyOutcome::Stalled,
                    "ReplicationFailed",
                    "snapshot-replication Job failed; see the Job/pod logs",
                    Some(SnapshotReplicationPhase::Failed),
                    Some(overlap),
                )
                .await?;
                // The Job is terminal, so no pod can still read the projected
                // copies — reclaim them now rather than only at the next idle.
                // Gated like the idle reap: with projection off, nothing was
                // ever projected under these prefixes (and a stale leftover
                // from an earlier opt-in is reaped by the next spawn's
                // resolve path), so the GETs would be guaranteed misses.
                if projection_enabled(repl) {
                    reap_srepl_projections(ctx, repl, namespace, name).await;
                }
                nudge_repositories_reverify(ctx, repl, name, namespace).await;
                Ok(Action::requeue(REQUEUE_FAILED))
            }
            None => Ok(Action::requeue(REQUEUE_RUNNING)),
        },
        None => {
            if has_active_snapshot_replication_job(&job_api, name).await? {
                return Ok(Action::requeue(REQUEUE_RUNNING));
            }
            spawn_snapshot_replication_job(
                ctx, namespace, name, &job_name, repl, source, dest, slot,
            )
            .await?;
            tracing::info!(replication = %name, slot = %slot.to_rfc3339(), "spawned snapshot-replication Job");
            Ok(Action::requeue(REQUEUE_RUNNING))
        }
    }
}

/// The stored `status.phase` label when it is one this build cannot read.
///
/// The pure seam behind the entry-time version-skew warning: `Some(label)`
/// exactly when a phase is recorded AND it decoded to
/// [`SnapshotReplicationPhase::Unknown`] (a newer operator's value, or legacy
/// stored data), `None` otherwise — including the no-status-yet case, which is
/// ordinary first-reconcile state and not skew.
///
/// The decision is delegated to
/// [`SnapshotReplicationPhase::is_unknown`], whose exhaustive `match` lives in
/// `kopiur_api`, so a phase variant added later cannot silently be treated as
/// unreadable here (and so this reads as a named predicate rather than an
/// `if let … Unknown(_)` probe the phase ratchet is blind to).
fn unreadable_phase(repl: &SnapshotReplication) -> Option<&str> {
    use kopiur_api::common::PhaseLabel;
    let phase = repl.status.as_ref()?.phase.as_ref()?;
    phase.is_unknown().then(|| phase.label())
}

/// Best-effort nudge asking BOTH of this replication's repositories to
/// re-verify their backends now (rather than on the next catalog refresh) —
/// unlike blob replication, both ends here are real repository CRs and either
/// backend may be the failing one. Called from the Job-failed arm
/// unconditionally: cheap, rate-limited (60s per repo), and Ready-gated inside
/// `request_repository_reverify` (#345). Best-effort by contract: an error is
/// logged and swallowed — a nudge failure must never mask the replication
/// failure that triggered it.
async fn nudge_repositories_reverify(
    ctx: &Context,
    repl: &SnapshotReplication,
    name: &str,
    namespace: &str,
) {
    for (side, repo_ref) in [
        ("source", &repl.spec.source_ref),
        ("destination", &repl.spec.destination_ref),
    ] {
        if let Err(e) =
            io::request_repository_reverify(&ctx.client, repo_ref, namespace, Utc::now()).await
        {
            tracing::debug!(
                replication = %name,
                side,
                error = %e,
                "repository reverify nudge failed (ignored)"
            );
        }
    }
}

// --- identity-overlap runtime check (issue #368, plan A7) --------------------

/// The runtime identity-overlap verdict. Pure so the decision is unit-tested;
/// the shared matching kernel is
/// [`kopiur_api::validate::replication_identity_overlap`] — the SAME function
/// the admission webhook evaluates, so what admission warned about and what the
/// reconcile stalls on cannot fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlapVerdict {
    /// No destination-side policy identity is selected by this replication.
    None,
    /// Overlap exists; the run proceeds with a warning condition.
    Warn {
        /// The overlapping identities (`username@hostname:path`), sorted.
        identities: Vec<String>,
    },
    /// Overlap exists AND `pruning: mirrorSource` — the run is skipped
    /// (`Ready` stalled) until the conflict is resolved.
    Stall {
        /// The overlapping identities (`username@hostname:path`), sorted.
        identities: Vec<String>,
    },
}

/// **Pure.** Evaluate the overlap between this replication's selection and the
/// destination-side policies' resolved identities, escalating to
/// [`OverlapVerdict::Stall`] under `pruning: mirrorSource` (the data-loss
/// combination: a bulk source-side vanish would cascade into identities the
/// destination does not merely mirror).
pub fn overlap_verdict(
    include: &[IdentityMatcher],
    exclude: &[IdentityMatcher],
    dest_policy_identities: &[kopiur_api::common::ResolvedIdentity],
    mirror_source: bool,
) -> OverlapVerdict {
    let identities =
        validate::replication_identity_overlap(include, exclude, dest_policy_identities);
    match (identities.is_empty(), mirror_source) {
        (true, _) => OverlapVerdict::None,
        (false, true) => OverlapVerdict::Stall { identities },
        (false, false) => OverlapVerdict::Warn { identities },
    }
}

/// **Pure.** The kopia identities a destination-side `SnapshotPolicy` writes
/// directly, from its live `status.resolved`: the resolved
/// `username@hostname` crossed with each resolved source's path (a policy with
/// no expanded sources yet contributes its identity as-is). A policy with no
/// resolved identity contributes nothing — it has never resolved, so there is
/// nothing on the destination to collide with yet.
pub fn policy_resolved_identities(
    policy: &SnapshotPolicy,
) -> Vec<kopiur_api::common::ResolvedIdentity> {
    let Some(resolved) = policy.status.as_ref().and_then(|s| s.resolved.as_ref()) else {
        return Vec::new();
    };
    let Some(id) = resolved.identity.as_ref() else {
        return Vec::new();
    };
    if resolved.sources.is_empty() {
        return vec![id.clone()];
    }
    resolved
        .sources
        .iter()
        .map(|s| kopiur_api::common::ResolvedIdentity {
            username: id.username.clone(),
            hostname: id.hostname.clone(),
            source_path: s.source_path.clone().or_else(|| id.source_path.clone()),
        })
        .collect()
}

/// **Pure.** Whether a `SnapshotPolicy`'s `spec.repository` targets this
/// replication's destination repository (borrowing the same ref matchers the
/// watch mappers use, so "targets" cannot mean two different things).
pub(crate) fn policy_targets_destination(
    policy: &SnapshotPolicy,
    dest_ref: &RepositoryRef,
    srepl_ns: &str,
) -> bool {
    // Any-of over every ref the policy names (tolerant iterator): a
    // multi-repo policy targets the destination when ANY ref matches.
    match dest_ref.kind {
        RepositoryKind::Repository => {
            let repo_ns = dest_ref.namespace.as_deref().unwrap_or(srepl_ns);
            kopiur_api::repository_refs(&policy.spec).any(|r| {
                crate::watch::ref_targets_repository(
                    r,
                    policy.namespace().as_deref(),
                    repo_ns,
                    &dest_ref.name,
                )
            })
        }
        RepositoryKind::ClusterRepository => kopiur_api::repository_refs(&policy.spec)
            .any(|r| crate::watch::ref_targets_cluster(r, &dest_ref.name)),
    }
}

/// List the destination-side policies and evaluate the overlap verdict. One
/// scope-appropriate LIST per pass — policies referencing the destination may
/// live in any namespace (cross-namespace refs are legal), so this cannot be a
/// single-namespace list in cluster scope.
async fn destination_identity_overlap(
    ctx: &Context,
    repl: &SnapshotReplication,
    namespace: &str,
) -> Result<OverlapVerdict> {
    let api: Api<SnapshotPolicy> = crate::controllers::scoped_api(&ctx.client, &ctx.watch_scope);
    let policies = api.list(&ListParams::default()).await?;
    let identities: Vec<kopiur_api::common::ResolvedIdentity> = policies
        .items
        .iter()
        .filter(|p| policy_targets_destination(p, &repl.spec.destination_ref, namespace))
        .flat_map(policy_resolved_identities)
        .collect();
    let (include, exclude) = selection_matchers(repl);
    Ok(overlap_verdict(
        include,
        exclude,
        &identities,
        mirror_source_pruning(repl),
    ))
}

/// The replication's include/exclude matcher slices (absent selection = both empty).
fn selection_matchers(repl: &SnapshotReplication) -> (&[IdentityMatcher], &[IdentityMatcher]) {
    match repl
        .spec
        .selection
        .as_ref()
        .and_then(|s| s.identities.as_ref())
    {
        Some(ids) => (ids.include.as_slice(), ids.exclude.as_slice()),
        None => (&[], &[]),
    }
}

/// Whether `spec.pruning` is the `mirrorSource` mode (exhaustive over [`Pruning`]).
fn mirror_source_pruning(repl: &SnapshotReplication) -> bool {
    match repl.spec.pruning.as_ref() {
        Some(Pruning::MirrorSource(_)) => true,
        Some(Pruning::None(_) | Pruning::Retention(_)) | None => false,
    }
}

/// A capped, deterministic identity list for condition messages.
fn identity_sample(identities: &[String]) -> String {
    const CAP: usize = 5;
    let mut out = identities
        .iter()
        .take(CAP)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if identities.len() > CAP {
        out.push_str(&format!(" (+{} more)", identities.len() - CAP));
    }
    out
}

// --- idle-arm heals ----------------------------------------------------------

/// The not-due heal pass: the ClusterRepository-destination duplicate reap and
/// the between-runs projected-credentials reap. Best-effort where safe;
/// propagates only the single-flight LIST error (the caller must not treat a
/// blind pass as settled).
async fn idle_heal(
    ctx: &Context,
    repl: &SnapshotReplication,
    job_api: &Api<Job>,
    namespace: &str,
    name: &str,
    dest: &ResolvedRepository,
) -> Result<()> {
    // Cross-namespace discovered duplicates exist only for a ClusterRepository
    // destination: a namespaced Repository's rows all live where the mover runs
    // (the mover already reaps those same-namespace).
    if matches!(
        repl.spec.destination_ref.kind,
        RepositoryKind::ClusterRepository
    ) {
        reap_cross_namespace_duplicates(ctx, name, &dest.owner_ref.uid).await;
    }
    // Projected credential copies are only needed while a mover Job can load
    // them via `envFrom`; reap them between runs so no live copy of either
    // repository's Secrets lingers (they are re-projected at the next spawn).
    // Gated on no active Job — reaping under an Active Job would strand a
    // retry pod in `CreateContainerConfigError` (the #103-class hazard).
    if projection_enabled(repl) && !has_active_snapshot_replication_job(job_api, name).await? {
        reap_srepl_projections(ctx, repl, namespace, name).await;
    }
    Ok(())
}

/// Whether this CR opted into credential projection.
fn projection_enabled(repl: &SnapshotReplication) -> bool {
    repl.spec
        .credential_projection
        .as_ref()
        .is_some_and(|c| c.enabled)
}

/// Best-effort reap of both credential-projection prefixes (`-srepl-src` /
/// `-srepl-dst`). Infallible: cleanup must never fail a reconcile.
async fn reap_srepl_projections(
    ctx: &Context,
    repl: &SnapshotReplication,
    namespace: &str,
    cr_name: &str,
) {
    use k8s_openapi::api::core::v1::Secret;
    let Some(uid) = repl.uid() else {
        return;
    };
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), namespace);
    let mut deleted = 0usize;
    for prefix in [
        io::CredsPrefix::snapshot_replication_src(cr_name),
        io::CredsPrefix::snapshot_replication_dst(cr_name),
    ] {
        let outcome = io::reap_projection(&secrets, &prefix, &uid, namespace, "run finished").await;
        deleted += outcome.deleted;
    }
    if deleted > 0 {
        ctx.metrics
            .inc_creds_secrets_reaped("terminal", deleted as u64);
    }
}

/// The reap-relevant view of one dest-repository catalog row (a `Snapshot` CR
/// carrying the destination repo's uid label). Pure data so the match decision
/// is unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateRow {
    /// The CR's namespace (a duplicate may live anywhere for a ClusterRepository).
    pub namespace: String,
    /// The CR name.
    pub name: String,
    /// `origin` LABEL, parsed totally ([`Origin::parse`] — unknown values are `None`).
    pub label_origin: Option<Origin>,
    /// `status.origin` — the controller-written provenance for the row itself.
    pub status_origin: Option<Origin>,
    /// The `snapshot-id` LABEL value.
    pub snapshot_id_label: Option<String>,
    /// `status.snapshot.kopiaSnapshotID` — the controller/mover-written
    /// provenance that CONFIRMS the label (labels alone are forgeable).
    pub status_snapshot_id: Option<String>,
}

/// **Pure.** The discovered duplicates to delete: a `discovered` row whose
/// snapshot-id label matches a `replicated` row's label AND that replicated
/// row's `status.snapshot.kopiaSnapshotID` CONFIRMS its label — status
/// provenance is the gate; a label anyone can stamp is not. The discovered
/// side's own status provenance must agree too (a mid-write row whose status
/// has not landed is conservatively spared). Unknown/absent origins are never
/// touched. Returns `(namespace, name)` pairs, in row order.
pub fn discovered_duplicate_names(rows: &[DuplicateRow]) -> Vec<(String, String)> {
    use std::collections::BTreeSet;
    let confirmed: BTreeSet<&str> = rows
        .iter()
        .filter(|r| {
            matches!(r.label_origin, Some(Origin::Replicated))
                && matches!(r.status_origin, Some(Origin::Replicated))
        })
        .filter_map(|r| {
            let label = r.snapshot_id_label.as_deref()?;
            (r.status_snapshot_id.as_deref() == Some(label)).then_some(label)
        })
        .collect();
    rows.iter()
        .filter(|r| {
            matches!(r.label_origin, Some(Origin::Discovered))
                && matches!(r.status_origin, Some(Origin::Discovered))
        })
        .filter(|r| {
            r.snapshot_id_label
                .as_deref()
                .is_some_and(|id| confirmed.contains(id))
        })
        .map(|r| (r.namespace.clone(), r.name.clone()))
        .collect()
}

/// Project a `Snapshot` CR into its [`DuplicateRow`].
fn duplicate_row(s: &Snapshot) -> DuplicateRow {
    let labels = s.labels();
    let status = s.status.as_ref();
    DuplicateRow {
        namespace: s.namespace().unwrap_or_default(),
        name: s.name_any(),
        label_origin: labels.get(ORIGIN_LABEL).and_then(|v| Origin::parse(v)),
        status_origin: status.and_then(|st| st.origin),
        snapshot_id_label: labels.get(SNAPSHOT_ID_LABEL).cloned(),
        status_snapshot_id: status
            .and_then(|st| st.snapshot.as_ref())
            .map(|i| i.kopia_snapshot_id.clone()),
    }
}

/// Reap discovered duplicates of confirmed replicated rows across ALL
/// namespaces (a `ClusterRepository` destination places discovered rows by
/// hostname, possibly outside the namespace the mover can delete in — plan A6).
/// Reads the shared `Snapshot` reflector, never an `Api::list` per pass, and
/// SKIPS entirely while that cache is cold (fail-safe: never delete off a
/// possibly-stale view). Deleting a discovered row never deletes kopia data
/// (discovered rows are forced `deletionPolicy: Retain`). Best-effort per row.
async fn reap_cross_namespace_duplicates(ctx: &Context, name: &str, dest_uid: &str) {
    use std::sync::atomic::Ordering;
    let Some(store) = ctx.snapshot_store.get() else {
        return;
    };
    if !ctx.snapshot_synced.load(Ordering::Acquire) {
        return;
    }
    let rows: Vec<DuplicateRow> = store
        .state()
        .iter()
        .filter(|s| s.labels().get(REPOSITORY_UID_LABEL).map(String::as_str) == Some(dest_uid))
        .map(|s| duplicate_row(s))
        .collect();
    for (ns, dup) in discovered_duplicate_names(&rows) {
        let api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), &ns);
        match api.delete(&dup, &DeleteParams::default()).await {
            Ok(_) => tracing::info!(
                replication = %name,
                snapshot = %format!("{ns}/{dup}"),
                "deleted cross-namespace discovered duplicate of a replicated copy"
            ),
            Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => {}
            Err(e) => tracing::warn!(
                replication = %name,
                snapshot = %format!("{ns}/{dup}"),
                error = %e,
                "cross-namespace duplicate reap failed (retried next idle pass)"
            ),
        }
    }
}

// --- Job spawn ---------------------------------------------------------------

/// Build + apply the per-slot snapshot-replication mover Job.
#[allow(clippy::too_many_arguments)]
async fn spawn_snapshot_replication_job(
    ctx: &Context,
    namespace: &str,
    cr_name: &str,
    job_name: &str,
    repl: &SnapshotReplication,
    source: &ResolvedRepository,
    dest: &ResolvedRepository,
    slot: DateTime<Utc>,
) -> Result<()> {
    // Defensive re-check of the admission rule (one validator, two callers): a
    // same-kind static/workload-identity auth mix would leak the static side's
    // env into the workload-identity side's ambient credential chain.
    if let Err(e) = kopiur_api::validate::validate_replication_auth(&source.backend, &dest.backend)
    {
        return Err(Error::Validation(e.to_string()));
    }
    let work_spec = build_snapshot_replication_work_spec(repl, source, dest, namespace, cr_name);

    let mut labels = BTreeMap::new();
    labels.insert(
        COMPONENT_LABEL.to_string(),
        SNAPSHOT_REPLICATION_COMPONENT.to_string(),
    );
    labels.insert(
        SNAPSHOT_REPLICATION_INSTANCE_LABEL.to_string(),
        cr_name.to_string(),
    );
    let mut annotations = BTreeMap::new();
    annotations.insert(
        SNAPSHOT_REPLICATION_SLOT_ANNOTATION.to_string(),
        slot.to_rfc3339(),
    );

    // One migrate pod touches BOTH backends. It runs under the DEDICATED
    // snapshot-replication mover SA/role: this mover creates and deletes
    // Snapshot CRs, verbs the generic mover SA must never hold.
    let mover_identity = io::ensure_snapshot_replication_mover_identity(
        &ctx.client,
        namespace,
        &[&source.backend, &dest.backend],
        &ctx.mover_clusterrole,
        ctx.mover_role_kind.as_str(),
    )
    .await?;
    mover_identity.decorate_labels(&mut labels);

    let (creds_secrets, extra_env) =
        resolve_srepl_creds(ctx, namespace, cr_name, repl, source, dest).await?;

    // A filesystem SOURCE repo needs its volume mounted — READ-ONLY: a
    // replication never writes to its source (migrate opens it read-only; the
    // persisted config/credentials live on the cache emptyDir, not the repo).
    let repo_volume =
        io::filesystem_repo_mount_source(&source.backend).map(|mount_source| VolumeMountSpec {
            source: mount_source,
            mount_path: io::filesystem_repo_path(&source.backend).unwrap_or_default(),
            read_only: true,
        });
    // A filesystem DESTINATION needs its volume mounted read-write — `kopia
    // snapshot migrate` writes the copies into it. Carried in the
    // `source_volume` slot (the Job builder just turns it into a pod
    // volume/mount at the destination's path; the two repositories are distinct
    // CRs whose paths the webhook keeps from colliding, so the two mounts never
    // fight). Object-store destinations reach the backend over the network.
    let dest_volume =
        io::filesystem_repo_mount_source(&dest.backend).map(|mount_source| VolumeMountSpec {
            source: mount_source,
            mount_path: io::filesystem_repo_path(&dest.backend).unwrap_or_default(),
            read_only: false,
        });
    let owner = io::owner_ref_for(repl, "SnapshotReplication")?;

    let resolved_mover = kopiur_api::common::resolve_mover(
        source.mover_defaults.as_ref(),
        repl.spec
            .mover
            .as_ref()
            .and_then(|m| m.security_context.as_ref()),
        repl.spec
            .mover
            .as_ref()
            .and_then(|m| m.pod_security_context.as_ref()),
        repl.spec.mover.as_ref().and_then(|m| m.resources.as_ref()),
        repl.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
        repl.spec
            .mover
            .as_ref()
            .and_then(|m| m.ttl_seconds_after_finished),
    );
    let limits = JobLimits {
        ttl_seconds_after_finished: resolved_mover
            .ttl_seconds_after_finished
            .or(Some(SNAPSHOT_REPLICATION_JOB_TTL_SECS)),
        ..JobLimits::default()
    };

    let inputs = MoverJobInputs {
        name: job_name,
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
        source_volume: dest_volume,
        repo_volume,
        creds_secrets,
        result_configmap: None,
        service_account: mover_identity.service_account.as_deref(),
        passthrough_env: ctx.mover_env_passthrough.clone(),
        extra_env,
        annotations,
        cache_volume: Default::default(),
        scratch_volume: None,
        readiness_exec: None,
    };
    let job = jobs::build_job(&inputs)?;
    io::apply_mover_objects(&ctx.client, namespace, job_name, None, &job).await?;
    Ok(())
}

/// Resolve BOTH repositories' mover credentials (projecting when opted-in),
/// verify presence, and assemble the Job's `envFrom` set plus the destination
/// password `extra_env`.
async fn resolve_srepl_creds(
    ctx: &Context,
    namespace: &str,
    cr_name: &str,
    repl: &SnapshotReplication,
    source: &ResolvedRepository,
    dest: &ResolvedRepository,
) -> Result<(
    Vec<jobs::CredsEnvFrom>,
    Vec<k8s_openapi::api::core::v1::EnvVar>,
)> {
    let owner = io::owner_ref_for(repl, "SnapshotReplication")?;
    let consumer_enabled = projection_enabled(repl);
    let src_creds = io::resolve_mover_creds_for(
        &ctx.client,
        namespace,
        &io::CredsPrefix::snapshot_replication_src(cr_name),
        &owner,
        source,
        consumer_enabled,
        io::repo_kind_str(repl.spec.source_ref.kind),
        &repl.spec.source_ref.name,
    )
    .await?;
    let dst_creds = io::resolve_mover_creds_for(
        &ctx.client,
        namespace,
        &io::CredsPrefix::snapshot_replication_dst(cr_name),
        &owner,
        dest,
        consumer_enabled,
        io::repo_kind_str(repl.spec.destination_ref.kind),
        &repl.spec.destination_ref.name,
    )
    .await?;
    let projected = src_creds.projected + dst_creds.projected;
    if projected > 0 {
        ctx.metrics.inc_secrets_projected(namespace, projected);
    }
    // Belt-and-braces: verify the RESOLVED names (verbatim or projected) exist
    // in the Job's namespace before launching a Job that would otherwise hang
    // on a missing-Secret `envFrom`.
    for (creds, repo_ref, repo) in [
        (&src_creds, &repl.spec.source_ref, source),
        (&dst_creds, &repl.spec.destination_ref, dest),
    ] {
        let creds_ctx = io::CredsContext {
            secret_names: &creds.names,
            repo_kind: io::repo_kind_str(repo_ref.kind),
            repo_name: &repo_ref.name,
            repo_secret_namespace: repo.encryption.password_secret_ref.namespace.as_deref(),
        };
        io::ensure_creds_present(&ctx.client, namespace, &creds_ctx).await?;
    }
    // The destination's kopia password rides a dedicated env var
    // (`KOPIUR_DEST_KOPIA_PASSWORD`) via valueFrom on the RESOLVED Secret —
    // when projection renamed the Secret, the projected copy's name.
    let (pw_name, pw_key) = dest_password_ref(&dst_creds.names, &dest.encryption)?;
    let extra_env = vec![dest_password_env(&pw_name, &pw_key)];
    Ok((
        snapshot_replication_creds_env_from(src_creds.names, dst_creds.names),
        extra_env,
    ))
}

/// Assemble the mover's `envFrom` credential set: the SOURCE repository's
/// Secrets verbatim (kopia reads the plain names at connect), plus EVERY
/// destination Secret under the
/// [`DEST_ENV_PREFIX`](kopiur_api::creds::DEST_ENV_PREFIX) so its keys can't
/// collide with the source's identically named ones. The destination entries
/// are appended WITHOUT deduping against the source: a Secret referenced by
/// both sides is loaded twice — once plain for the source, once prefixed for
/// the destination — because kopia reads the two copies under different
/// env-var names (the `RepositoryReplication` loaded-twice contract, #200).
fn snapshot_replication_creds_env_from(
    source_names: Vec<String>,
    dest_names: Vec<String>,
) -> Vec<jobs::CredsEnvFrom> {
    let mut creds = io::plain_creds(source_names);
    creds.extend(
        dest_names
            .into_iter()
            .map(|n| jobs::CredsEnvFrom::prefixed(n, kopiur_api::creds::DEST_ENV_PREFIX)),
    );
    creds
}

/// **Pure.** The `(name, key)` of the destination password Secret AS THE JOB
/// LOADS IT: `resolved_names[0]` — [`io::mover_creds_secret_refs`] yields the
/// encryption-password ref FIRST (order-stable, pinned by its tests) and
/// `resolve_mover_creds` preserves ref order, so index 0 is the password
/// Secret's resolved name, which is the PROJECTED copy's name whenever
/// projection renamed it (the copy carries the same keys verbatim). The key is
/// the spec's `passwordSecretRef.key`, defaulting to
/// [`io::DEFAULT_PASSWORD_KEY`].
fn dest_password_ref(
    resolved_names: &[String],
    encryption: &Encryption,
) -> Result<(String, String)> {
    let name = resolved_names.first().cloned().ok_or_else(|| {
        Error::Invariant(
            "destination credential resolution yielded no Secret names — the encryption \
             password Secret is mandatory, so this is a kopiur bug"
                .into(),
        )
    })?;
    let key = encryption
        .password_secret_ref
        .key
        .clone()
        .unwrap_or_else(|| io::DEFAULT_PASSWORD_KEY.to_string());
    Ok((name, key))
}

/// **Pure.** The `KOPIUR_DEST_KOPIA_PASSWORD` Job env var: a
/// `valueFrom.secretKeyRef` on the resolved destination password Secret, so
/// the plaintext never rides the Job spec.
fn dest_password_env(name: &str, key: &str) -> k8s_openapi::api::core::v1::EnvVar {
    use k8s_openapi::api::core::v1::{EnvVar, EnvVarSource, SecretKeySelector};
    EnvVar {
        name: kopiur_api::creds::DEST_KOPIA_PASSWORD_ENV.to_string(),
        value: None,
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: name.to_string(),
                key: key.to_string(),
                optional: Some(false),
            }),
            ..Default::default()
        }),
    }
}

/// Build the snapshot-replication mover work spec. Pure (no IO) so the
/// spec → op mapping is unit-testable: connect to the SOURCE repository,
/// migrate the selection into the destination.
pub fn build_snapshot_replication_work_spec(
    repl: &SnapshotReplication,
    source: &ResolvedRepository,
    dest: &ResolvedRepository,
    namespace: &str,
    cr_name: &str,
) -> MoverWorkSpec {
    let selection = repl.spec.selection.as_ref();
    let (include, exclude) = selection_matchers(repl);
    let migrate = repl.spec.migrate.unwrap_or_default();
    MoverWorkSpec {
        version: 1,
        operation: Operation::SnapshotReplicate(SnapshotReplicateOp {
            destination: backend_to_repository_connect(&dest.backend, dest.ca_bundle_pem.clone()),
            destination_repository: ReplicationRepositoryRef {
                kind: io::repo_kind_str(repl.spec.destination_ref.kind).to_string(),
                name: repl.spec.destination_ref.name.clone(),
                namespace: dest.repo_namespace.clone(),
                uid: dest.owner_ref.uid.clone(),
            },
            source_repository: ReplicationSourceRef {
                kind: io::repo_kind_str(repl.spec.source_ref.kind).to_string(),
                name: repl.spec.source_ref.name.clone(),
                namespace: source.repo_namespace.clone(),
            },
            include: include.iter().map(matcher_spec).collect(),
            exclude: exclude.iter().map(matcher_spec).collect(),
            latest_only: selection.map(|s| s.latest_only).unwrap_or(false),
            parallel: migrate.parallel,
            policies: policy_copy_mode_spec(migrate.policies),
            pruning: pruning_spec(repl.spec.pruning.as_ref()),
        }),
        // Replication does not snapshot; a stable sentinel identity (like
        // maintenance / blob replication).
        identity: ResolvedIdentity {
            username: "kopiur-snapshot-replication".to_string(),
            hostname: namespace.to_string(),
            source_path: String::new(),
        },
        repository: backend_to_repository_connect(&source.backend, source.ca_bundle_pem.clone()),
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            kind: "SnapshotReplication".to_string(),
            name: cr_name.to_string(),
            namespace: namespace.to_string(),
        },
        hook_plan: Default::default(),
        options: MoverOptions::default(),
        cache: Default::default(),
        throttle: io::throttle_spec(source.mover_defaults.as_ref()),
    }
}

/// CRD [`IdentityMatcher`] → wire [`IdentityMatcherSpec`] (a field-for-field
/// mirror; the wire type exists so the mover contract can't drift with CRD
/// refactors).
fn matcher_spec(m: &IdentityMatcher) -> IdentityMatcherSpec {
    IdentityMatcherSpec {
        username: m.username.clone(),
        hostname: m.hostname.clone(),
        source_path: m.source_path.clone(),
    }
}

/// CRD [`PolicyCopyMode`] → wire [`PolicyCopyModeSpec`]. Exhaustive — a new
/// mode cannot compile until mapped.
fn policy_copy_mode_spec(mode: PolicyCopyMode) -> PolicyCopyModeSpec {
    match mode {
        PolicyCopyMode::None => PolicyCopyModeSpec::None,
        PolicyCopyMode::Copy => PolicyCopyModeSpec::Copy,
        PolicyCopyMode::CopyOverwrite => PolicyCopyModeSpec::CopyOverwrite,
    }
}

/// CRD [`Pruning`] → wire [`PruningSpec`]. Exhaustive; absent = never prune.
fn pruning_spec(pruning: Option<&Pruning>) -> PruningSpec {
    match pruning {
        None | Some(Pruning::None(_)) => PruningSpec::None(NoPruningSpec {}),
        Some(Pruning::MirrorSource(_)) => PruningSpec::MirrorSource(MirrorSourcePruningSpec {}),
        Some(Pruning::Retention(r)) => PruningSpec::Retention(ReplicationRetentionSpec {
            keep_latest: r.keep_latest,
            keep_hourly: r.keep_hourly,
            keep_daily: r.keep_daily,
            keep_weekly: r.keep_weekly,
            keep_monthly: r.keep_monthly,
            keep_annual: r.keep_annual,
        }),
    }
}

// --- scheduling kernel (clone of repository_replication's) -------------------

/// The replication slot due now (cron + jitter strictly after the last run), or
/// `None` if not yet due. Pure given the CR, `now`, and the SOURCE repository's
/// `scheduleDefaults.timezone` (`repo_tz`).
pub fn due_slot(
    repl: &SnapshotReplication,
    now: DateTime<Utc>,
    repo_tz: Option<&str>,
) -> Option<DateTime<Utc>> {
    let after = last_run_at(repl).unwrap_or_else(|| now - chrono::Duration::days(365));
    match slot_for(repl, after, repo_tz) {
        Ok(slot) if now >= slot => Some(slot),
        _ => None,
    }
}

/// The next cron slot for this replication strictly after `after` (croner +
/// jitter, seeded by the CR UID). `spec.schedule.timezone` wins; else the
/// SOURCE repository's `scheduleDefaults.timezone`; else UTC.
fn slot_for(
    repl: &SnapshotReplication,
    after: DateTime<Utc>,
    repo_tz: Option<&str>,
) -> Result<DateTime<Utc>> {
    let seed = repl.uid().unwrap_or_else(|| repl.name_any());
    let jitter = repl
        .spec
        .schedule
        .jitter
        .as_deref()
        .and_then(parse_go_duration);
    let tz = kopiur_api::common::resolve_tz_with_default(
        repl.spec.schedule.timezone.as_deref(),
        repo_tz,
    );
    next_fire(&repl.spec.schedule.cron, jitter, &seed, after, tz)
}

/// Parse `status.lastReplicated` (RFC3339) into a `DateTime<Utc>`.
fn last_run_at(repl: &SnapshotReplication) -> Option<DateTime<Utc>> {
    repl.status
        .as_ref()
        .and_then(|s| s.last_replicated.as_deref())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// How long until the next replication slot. When `handled` is set, that slot is
/// the search anchor (so a just-handled slot doesn't immediately re-fire).
/// Floored at the running cadence, capped by the caller.
fn next_wakeup(
    repl: &SnapshotReplication,
    now: DateTime<Utc>,
    handled: Option<DateTime<Utc>>,
    repo_tz: Option<&str>,
) -> Duration {
    let after = handled.unwrap_or_else(|| last_run_at(repl).unwrap_or(now));
    match slot_for(repl, after, repo_tz) {
        Ok(slot) if slot > now => (slot - now)
            .to_std()
            .unwrap_or(REQUEUE_CAP)
            .max(REQUEUE_RUNNING),
        _ => REQUEUE_RUNNING,
    }
}

/// Cap a requeue so the schedule/readiness is re-evaluated within the heartbeat.
fn cap(d: Duration) -> Duration {
    d.min(REQUEUE_CAP)
}

/// Deterministic, ≤52-char, DNS-1123-safe per-slot Job name:
/// `<cr>-srepl-<unix_slot>` (truncate+hash long names, like maintenance).
fn snapshot_replication_job_name(cr: &str, slot: DateTime<Utc>) -> String {
    const MAX: usize = 52;
    let suffix = format!("-srepl-{}", slot.timestamp());
    let budget = MAX.saturating_sub(suffix.len());
    if cr.len() <= budget {
        format!("{cr}{suffix}")
    } else {
        let hash = short_hash(cr);
        let keep = budget.saturating_sub(hash.len() + 1);
        let trunc: String = cr.chars().take(keep).collect();
        format!("{trunc}-{hash}{suffix}")
    }
}

/// Whether any non-terminal snapshot-replication Job is owned by this CR
/// (single-flight gate).
async fn has_active_snapshot_replication_job(job_api: &Api<Job>, cr_name: &str) -> Result<bool> {
    let selector = format!(
        "{COMPONENT_LABEL}={SNAPSHOT_REPLICATION_COMPONENT},{SNAPSHOT_REPLICATION_INSTANCE_LABEL}={cr_name}"
    );
    let jobs = job_api
        .list(&ListParams::default().labels(&selector))
        .await?;
    Ok(jobs.items.iter().any(|j| job_terminal_state(j).is_none()))
}

// --- status writer ------------------------------------------------------------

/// The `IdentityOverlap` condition triple `(status, reason, message)` a verdict
/// writes. Exhaustive over [`OverlapVerdict`].
fn overlap_condition_fields(v: &OverlapVerdict) -> (&'static str, &'static str, String) {
    match v {
        OverlapVerdict::None => (
            "False",
            NO_IDENTITY_OVERLAP_REASON,
            "no destination-side SnapshotPolicy identity is selected by this replication"
                .to_string(),
        ),
        OverlapVerdict::Warn { identities } | OverlapVerdict::Stall { identities } => (
            "True",
            IDENTITY_OVERLAP_REASON,
            format!(
                "a destination-side SnapshotPolicy writes directly under identities this \
                 replication also selects ({}); replicated copies and direct backups will \
                 interleave in one kopia identity's history. Narrow spec.selection or \
                 re-identify the destination policy",
                identity_sample(identities)
            ),
        ),
    }
}

/// Patch the kstatus Ready conditions (+ optional phase and, when a verdict was
/// computed this pass, the `IdentityOverlap` condition) only when something it
/// writes changed, so the reconcile does not hot-loop on its own status writes
/// (transition-guarded).
///
/// The overlap condition rides the SAME patch as `Ready` — a second writer in
/// one reconcile would compute its array from the stale `repl.status` snapshot
/// and erase the first write (the condition-writers-clobber hazard). Arms that
/// run BEFORE the verdict exists pass `None` and leave the stored condition
/// untouched.
///
/// `phase` is TYPED, not a `&str`: the wire value comes from
/// [`PhaseLabel::label`](kopiur_api::common::PhaseLabel::label), the same
/// definition `SnapshotReplicationStatus` decodes with, so a renamed variant is
/// a compile error here instead of a string that silently stops matching what
/// anyone reads back.
#[allow(clippy::too_many_arguments)]
async fn patch_ready_if_changed(
    api: &Api<SnapshotReplication>,
    name: &str,
    repl: &SnapshotReplication,
    outcome: io::ReadyOutcome,
    reason: &str,
    message: &str,
    phase: Option<SnapshotReplicationPhase>,
    overlap: Option<&OverlapVerdict>,
) -> Result<()> {
    use kopiur_api::common::PhaseLabel;
    let existing: Vec<_> = repl
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    let current = existing
        .iter()
        .find(|c| c.type_ == "Ready")
        .map(|c| (c.status.clone(), c.reason.clone()));
    let target_status = match outcome {
        io::ReadyOutcome::Ready => "True",
        _ => "False",
    };
    let overlap_target = overlap.map(overlap_condition_fields);
    let overlap_unchanged = match &overlap_target {
        None => true,
        Some((st, oreason, omessage)) => existing
            .iter()
            .find(|c| c.type_ == IDENTITY_OVERLAP_CONDITION)
            .is_some_and(|c| c.status == *st && c.reason == *oreason && c.message == *omessage),
    };
    if overlap_unchanged
        && current.as_ref() == Some(&(target_status.to_string(), reason.to_string()))
    {
        return Ok(());
    }
    let observed_gen = repl.metadata.generation.unwrap_or(0);
    let mut conditions = io::set_ready(&existing, Some(observed_gen), outcome, reason, message);
    if let Some((st, oreason, omessage)) = &overlap_target {
        conditions = io::upsert_condition_status(
            &conditions,
            IDENTITY_OVERLAP_CONDITION,
            st,
            oreason,
            omessage,
            Some(observed_gen),
        );
    }
    let mut status = serde_json::json!({
        "observedGeneration": observed_gen,
        "conditions": conditions,
    });
    if let Some(p) = phase {
        status["phase"] = serde_json::json!(p.label());
    }
    io::patch_status(api, name, status).await?;
    Ok(())
}

/// `error_policy` for the `SnapshotReplication` controller.
pub fn error_policy(
    obj: std::sync::Arc<SnapshotReplication>,
    err: &Error,
    ctx: std::sync::Arc<Context>,
) -> Action {
    error_policy_for("SnapshotReplication", obj.as_ref(), err, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
    use kopiur_api::backend::{Backend, FilesystemBackend, S3Backend};
    use kopiur_api::common::{CronSpec, Encryption as Enc, RepositoryMode, SecretKeyRef};
    use kopiur_api::snapshot_replication::{
        IdentitySelection, MigrateOptions, MirrorSourcePruning, NoPruning, SelectionSpec,
    };
    use kopiur_api::{SnapshotReplicationSpec, SnapshotReplicationStatus};

    fn srepl_with(cron: &str, status: Option<SnapshotReplicationStatus>) -> SnapshotReplication {
        let mut r = SnapshotReplication::new(
            "offsite",
            SnapshotReplicationSpec {
                source_ref: RepositoryRef {
                    kind: RepositoryKind::Repository,
                    name: "nas-primary".into(),
                    namespace: None,
                },
                destination_ref: RepositoryRef {
                    kind: RepositoryKind::ClusterRepository,
                    name: "offsite-shared".into(),
                    namespace: None,
                },
                schedule: CronSpec {
                    cron: cron.into(),
                    jitter: None,
                    timezone: None,
                },
                selection: None,
                migrate: None,
                pruning: None,
                mover: None,
                credential_projection: None,
                suspend: false,
            },
        );
        r.metadata.uid = Some("uid-srepl-1".into());
        r.status = status;
        r
    }

    fn resolved_repo(backend: Backend, uid: &str, ns: Option<&str>) -> ResolvedRepository {
        ResolvedRepository {
            backend,
            encryption: Enc {
                password_secret_ref: SecretKeyRef {
                    name: "s".into(),
                    namespace: None,
                    key: None,
                },
            },
            repo_namespace: ns.map(str::to_string),
            mover_defaults: None,
            identity_defaults: None,
            schedule_defaults: None,
            on_namespace_delete: Default::default(),
            credential_projection_allowed: false,
            owner_ref: OwnerReference {
                uid: uid.into(),
                ..Default::default()
            },
            mode: RepositoryMode::ReadWrite,
            deletion_protection: None,
            mass_deletion_ack: None,
            catalog: None,
            ca_bundle_pem: None,
        }
    }

    fn sample_source() -> ResolvedRepository {
        resolved_repo(
            Backend::Filesystem(FilesystemBackend {
                path: "/repo".into(),
                volume: None,
            }),
            "src-uid",
            Some("ns"),
        )
    }

    fn sample_dest() -> ResolvedRepository {
        resolved_repo(
            Backend::S3(S3Backend {
                bucket: "mirror".into(),
                prefix: None,
                endpoint: None,
                region: None,
                auth: None,
                tls: None,
            }),
            "dest-uid",
            None,
        )
    }

    /// The skew seam: only a phase this build cannot decode is named, and the
    /// ordinary states (no status, no phase, a known phase) stay quiet — a
    /// warning on every pass of a healthy replication would be noise nobody
    /// reads, which is how a real skew gets missed.
    #[test]
    fn only_an_undecodable_phase_is_named_as_skew() {
        let none_at_all = srepl_with("0 6 * * *", None);
        assert_eq!(unreadable_phase(&none_at_all), None, "no status yet");

        let no_phase = srepl_with("0 6 * * *", Some(SnapshotReplicationStatus::default()));
        assert_eq!(unreadable_phase(&no_phase), None, "status without a phase");

        for known in [
            SnapshotReplicationPhase::Pending,
            SnapshotReplicationPhase::Replicating,
            SnapshotReplicationPhase::Succeeded,
            SnapshotReplicationPhase::Failed,
            SnapshotReplicationPhase::Suspended,
        ] {
            let r = srepl_with(
                "0 6 * * *",
                Some(SnapshotReplicationStatus {
                    phase: Some(known.clone()),
                    ..Default::default()
                }),
            );
            assert_eq!(unreadable_phase(&r), None, "{known:?} is readable");
        }

        // What a NEWER operator wrote: decoded verbatim into `Unknown`, and
        // reported with the raw string so the log names the actual value.
        let skewed = srepl_with(
            "0 6 * * *",
            Some(SnapshotReplicationStatus {
                phase: Some(SnapshotReplicationPhase::Unknown("Verifying".into())),
                ..Default::default()
            }),
        );
        assert_eq!(unreadable_phase(&skewed), Some("Verifying"));
    }

    // --- scheduling kernel -----------------------------------------------

    #[test]
    fn first_ever_reconcile_is_due() {
        let r = srepl_with("0 6 * * *", None);
        assert!(due_slot(&r, Utc::now(), None).is_some());
    }

    #[test]
    fn not_due_right_after_a_run() {
        let now = Utc::now();
        let just = (now - chrono::Duration::seconds(1)).to_rfc3339();
        let status = SnapshotReplicationStatus {
            last_replicated: Some(just),
            ..Default::default()
        };
        let r = srepl_with("0 6 * * *", Some(status));
        assert!(
            due_slot(&r, now, None).is_none(),
            "a replication that just ran must not be immediately due again"
        );
    }

    #[test]
    fn requeue_is_capped() {
        let now = Utc::now();
        let just = (now - chrono::Duration::seconds(1)).to_rfc3339();
        let status = SnapshotReplicationStatus {
            last_replicated: Some(just),
            ..Default::default()
        };
        let r = srepl_with("0 6 * * *", Some(status));
        assert!(cap(next_wakeup(&r, now, None, None)) <= REQUEUE_CAP);
    }

    #[test]
    fn due_slot_honors_source_repo_schedule_default_timezone() {
        // No own timezone on the schedule, so the SOURCE repository's default
        // must be what shifts the evaluated slot (tz precedence: spec → source
        // repo scheduleDefaults → UTC).
        let now = DateTime::parse_from_rfc3339("2026-06-09T06:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let just = (now - chrono::Duration::hours(2)).to_rfc3339();
        let status = SnapshotReplicationStatus {
            last_replicated: Some(just),
            ..Default::default()
        };
        let r = srepl_with("0 6 * * *", Some(status));
        assert!(
            due_slot(&r, now, None).is_some(),
            "UTC (no repo default) → 06:00 UTC has already passed"
        );
        assert!(
            due_slot(&r, now, Some("America/Los_Angeles")).is_none(),
            "the source repo's scheduleDefaults.timezone must shift the evaluated slot"
        );
    }

    #[test]
    fn job_name_deterministic_and_bounded() {
        let slot = DateTime::parse_from_rfc3339("2026-06-09T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let n = snapshot_replication_job_name("offsite", slot);
        assert!(n.len() <= 52);
        assert!(n.starts_with("offsite-srepl-"));
        assert_eq!(n, snapshot_replication_job_name("offsite", slot));
        let long = "a-very-long-snapshot-replication-name-blowing-the-dns-budget";
        assert!(snapshot_replication_job_name(long, slot).len() <= 52);
    }

    // --- work-spec field map ------------------------------------------------

    #[test]
    fn work_spec_maps_source_destination_and_identity_sentinel() {
        let r = srepl_with("0 6 * * *", None);
        let ws = build_snapshot_replication_work_spec(
            &r,
            &sample_source(),
            &sample_dest(),
            "ns",
            "offsite",
        );
        match &ws.operation {
            Operation::SnapshotReplicate(op) => {
                assert_eq!(op.destination.kind_str(), "S3");
                assert_eq!(op.destination_repository.kind, "ClusterRepository");
                assert_eq!(op.destination_repository.name, "offsite-shared");
                assert_eq!(op.destination_repository.namespace, None);
                assert_eq!(op.destination_repository.uid, "dest-uid");
                assert_eq!(op.source_repository.kind, "Repository");
                assert_eq!(op.source_repository.name, "nas-primary");
                assert_eq!(op.source_repository.namespace.as_deref(), Some("ns"));
                // No selection/migrate/pruning set → every knob defaults.
                assert!(op.include.is_empty());
                assert!(op.exclude.is_empty());
                assert!(!op.latest_only);
                assert_eq!(op.parallel, None);
                assert_eq!(op.policies, PolicyCopyModeSpec::None);
                assert!(op.pruning.is_none());
            }
            other => panic!("expected snapshot-replicate op, got {}", other.kind_str()),
        }
        // The work spec connects to the SOURCE; the sentinel identity never
        // collides with a real backup identity.
        assert_eq!(ws.repository.kind_str(), "Filesystem");
        assert_eq!(ws.identity.username, "kopiur-snapshot-replication");
        assert_eq!(ws.identity.hostname, "ns");
        assert_eq!(ws.identity.source_path, "");
        assert_eq!(ws.target_ref.kind, "SnapshotReplication");
        assert_eq!(ws.target_ref.name, "offsite");
    }

    #[test]
    fn work_spec_maps_every_selection_migrate_and_pruning_field_to_the_op() {
        // The controller-glue guard (the #216 bug class): every spec field must
        // land on the corresponding op field — plumbing that exists but a
        // hardcoded default never reaching it is exactly what this kills.
        let mut r = srepl_with("0 6 * * *", None);
        r.spec.selection = Some(SelectionSpec {
            identities: Some(IdentitySelection {
                include: vec![IdentityMatcher {
                    username: Some("pg-*".into()),
                    hostname: Some("billing".into()),
                    source_path: None,
                }],
                exclude: vec![IdentityMatcher {
                    source_path: Some("/scratch/*".into()),
                    ..Default::default()
                }],
            }),
            latest_only: true,
        });
        r.spec.migrate = Some(MigrateOptions {
            parallel: Some(4),
            policies: PolicyCopyMode::CopyOverwrite,
        });
        r.spec.pruning = Some(Pruning::Retention(
            serde_json::from_value(serde_json::json!({ "keepDaily": 7, "keepWeekly": 4 })).unwrap(),
        ));
        let ws = build_snapshot_replication_work_spec(
            &r,
            &sample_source(),
            &sample_dest(),
            "ns",
            "offsite",
        );
        match &ws.operation {
            Operation::SnapshotReplicate(op) => {
                assert_eq!(op.include.len(), 1);
                assert_eq!(op.include[0].username.as_deref(), Some("pg-*"));
                assert_eq!(op.include[0].hostname.as_deref(), Some("billing"));
                assert_eq!(op.exclude.len(), 1);
                assert_eq!(op.exclude[0].source_path.as_deref(), Some("/scratch/*"));
                assert!(op.latest_only);
                assert_eq!(op.parallel, Some(4));
                assert_eq!(op.policies, PolicyCopyModeSpec::CopyOverwrite);
                match &op.pruning {
                    PruningSpec::Retention(ret) => {
                        assert_eq!(ret.keep_daily, Some(7));
                        assert_eq!(ret.keep_weekly, Some(4));
                    }
                    other => panic!("expected retention pruning, got {other:?}"),
                }
            }
            other => panic!("expected snapshot-replicate op, got {}", other.kind_str()),
        }
    }

    #[test]
    fn pruning_spec_maps_every_mode() {
        assert!(pruning_spec(None).is_none());
        assert!(pruning_spec(Some(&Pruning::None(NoPruning {}))).is_none());
        assert!(matches!(
            pruning_spec(Some(&Pruning::MirrorSource(MirrorSourcePruning {}))),
            PruningSpec::MirrorSource(_)
        ));
    }

    // --- creds assembly -------------------------------------------------------

    #[test]
    fn creds_put_source_plain_and_every_destination_secret_prefixed() {
        let out = snapshot_replication_creds_env_from(
            vec!["src-pw".into(), "src-s3".into()],
            vec!["dst-pw".into(), "dst-s3".into()],
        );
        assert_eq!(
            out,
            vec![
                jobs::CredsEnvFrom::plain("src-pw"),
                jobs::CredsEnvFrom::plain("src-s3"),
                jobs::CredsEnvFrom::prefixed("dst-pw", "KOPIUR_DEST_"),
                jobs::CredsEnvFrom::prefixed("dst-s3", "KOPIUR_DEST_"),
            ]
        );
    }

    #[test]
    fn creds_shared_secret_is_loaded_twice_not_deduped() {
        // Same Secret name on both sides must appear BOTH plain (source) and
        // prefixed (destination) — kopia reads the two copies under different
        // env-var names, so collapsing them to one entry would strip the
        // destination's credentials (the #200 loaded-twice contract).
        let out = snapshot_replication_creds_env_from(vec!["shared".into()], vec!["shared".into()]);
        assert_eq!(
            out,
            vec![
                jobs::CredsEnvFrom::plain("shared"),
                jobs::CredsEnvFrom::prefixed("shared", "KOPIUR_DEST_"),
            ]
        );
    }

    #[test]
    fn dest_password_ref_picks_the_first_resolved_name_and_the_spec_key() {
        let enc = |key: Option<&str>| Enc {
            password_secret_ref: SecretKeyRef {
                name: "dst-pw".into(),
                namespace: None,
                key: key.map(str::to_string),
            },
        };
        // Verbatim name, default key.
        let (name, key) =
            dest_password_ref(&["dst-pw".to_string(), "dst-s3".to_string()], &enc(None)).unwrap();
        assert_eq!(name, "dst-pw");
        assert_eq!(key, io::DEFAULT_PASSWORD_KEY);
        // A PROJECTED (renamed) copy: the resolved name wins over the spec ref;
        // the copy carries the source's keys verbatim, so the key is unchanged.
        let (name, key) = dest_password_ref(
            &["offsite-srepl-dst-creds-0".to_string()],
            &enc(Some("password")),
        )
        .unwrap();
        assert_eq!(name, "offsite-srepl-dst-creds-0");
        assert_eq!(key, "password");
        // No names at all is an invariant violation, not a silent skip.
        assert!(dest_password_ref(&[], &enc(None)).is_err());
    }

    #[test]
    fn dest_password_env_is_a_value_from_secret_key_ref() {
        let env = dest_password_env("offsite-srepl-dst-creds-0", "KOPIA_PASSWORD");
        assert_eq!(env.name, "KOPIUR_DEST_KOPIA_PASSWORD");
        assert!(
            env.value.is_none(),
            "plaintext must never ride the Job spec"
        );
        let sel = env
            .value_from
            .as_ref()
            .and_then(|v| v.secret_key_ref.as_ref())
            .expect("secretKeyRef");
        assert_eq!(sel.name, "offsite-srepl-dst-creds-0");
        assert_eq!(sel.key, "KOPIA_PASSWORD");
        assert_eq!(sel.optional, Some(false));
    }

    // --- identity-overlap verdict ---------------------------------------------

    fn dest_id(u: &str, h: &str, p: &str) -> kopiur_api::common::ResolvedIdentity {
        kopiur_api::common::ResolvedIdentity {
            username: u.into(),
            hostname: h.into(),
            source_path: Some(p.into()),
        }
    }

    #[test]
    fn empty_selection_overlaps_every_destination_identity() {
        let ids = [dest_id("pg", "billing", "/pvc/data")];
        match overlap_verdict(&[], &[], &ids, false) {
            OverlapVerdict::Warn { identities } => {
                assert_eq!(identities, vec!["pg@billing:/pvc/data".to_string()]);
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn overlap_with_mirror_source_pruning_stalls() {
        let ids = [dest_id("pg", "billing", "/pvc/data")];
        assert!(matches!(
            overlap_verdict(&[], &[], &ids, true),
            OverlapVerdict::Stall { .. }
        ));
    }

    #[test]
    fn include_glob_narrows_and_exclude_clears_the_overlap() {
        let ids = [
            dest_id("pg-main", "billing", "/pvc/data"),
            dest_id("redis", "cache", "/pvc/redis"),
        ];
        let include = [IdentityMatcher {
            username: Some("pg-*".into()),
            ..Default::default()
        }];
        match overlap_verdict(&include, &[], &ids, false) {
            OverlapVerdict::Warn { identities } => {
                assert_eq!(identities, vec!["pg-main@billing:/pvc/data".to_string()]);
            }
            other => panic!("expected Warn, got {other:?}"),
        }
        // Excluding the overlapping identity clears the verdict entirely.
        let exclude = [IdentityMatcher {
            username: Some("pg-*".into()),
            ..Default::default()
        }];
        assert_eq!(
            overlap_verdict(&include, &exclude, &ids, true),
            OverlapVerdict::None,
            "exclude wins — even under mirrorSource there is nothing to stall on"
        );
        // No destination identities at all: nothing to overlap.
        assert_eq!(overlap_verdict(&[], &[], &[], true), OverlapVerdict::None);
    }

    #[test]
    fn policy_identities_cross_the_resolved_identity_with_every_source() {
        let policy: SnapshotPolicy = serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotPolicy",
            "metadata": { "name": "daily", "namespace": "apps" },
            "spec": {
                "repository": { "name": "offsite-shared", "kind": "ClusterRepository" },
                "sources": [ { "pvc": { "name": "data" } } ],
            },
            "status": {
                "resolved": {
                    "identity": { "username": "pg", "hostname": "apps" },
                    "sources": [
                        { "pvc": "apps/data", "sourcePath": "/pvc/data" },
                        { "pvc": "apps/wal", "sourcePath": "/pvc/wal" },
                    ],
                },
            },
        }))
        .expect("policy fixture");
        let ids = policy_resolved_identities(&policy);
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0].username, "pg");
        assert_eq!(ids[0].source_path.as_deref(), Some("/pvc/data"));
        assert_eq!(ids[1].source_path.as_deref(), Some("/pvc/wal"));

        // A never-resolved policy contributes nothing to collide with.
        let unresolved: SnapshotPolicy = serde_json::from_value(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotPolicy",
            "metadata": { "name": "new", "namespace": "apps" },
            "spec": {
                "repository": { "name": "offsite-shared", "kind": "ClusterRepository" },
                "sources": [ { "pvc": { "name": "data" } } ],
            },
        }))
        .expect("policy fixture");
        assert!(policy_resolved_identities(&unresolved).is_empty());

        // The destination matcher: this policy targets the ClusterRepository dest.
        let dest_ref = RepositoryRef {
            kind: RepositoryKind::ClusterRepository,
            name: "offsite-shared".into(),
            namespace: None,
        };
        assert!(policy_targets_destination(&policy, &dest_ref, "backups"));
        let other = RepositoryRef {
            kind: RepositoryKind::Repository,
            name: "offsite-shared".into(),
            namespace: None,
        };
        assert!(
            !policy_targets_destination(&policy, &other, "backups"),
            "a namespaced dest ref never matches a ClusterRepository policy ref"
        );
    }

    // --- cross-namespace discovered-duplicate reap ------------------------------

    fn dup(
        ns: &str,
        name: &str,
        label_origin: Option<Origin>,
        status_origin: Option<Origin>,
        id_label: Option<&str>,
        status_id: Option<&str>,
    ) -> DuplicateRow {
        DuplicateRow {
            namespace: ns.into(),
            name: name.into(),
            label_origin,
            status_origin,
            snapshot_id_label: id_label.map(str::to_string),
            status_snapshot_id: status_id.map(str::to_string),
        }
    }

    #[test]
    fn duplicate_reap_deletes_only_status_confirmed_matches() {
        let rows = vec![
            // A replicated row whose STATUS confirms its label: the anchor.
            dup(
                "backups",
                "copy-a",
                Some(Origin::Replicated),
                Some(Origin::Replicated),
                Some("k1aaa"),
                Some("k1aaa"),
            ),
            // Its discovered duplicate in ANOTHER namespace: deleted.
            dup(
                "media",
                "disc-a",
                Some(Origin::Discovered),
                Some(Origin::Discovered),
                Some("k1aaa"),
                Some("k1aaa"),
            ),
            // A replicated row whose status does NOT confirm the label (forged
            // or mid-write): its "duplicate" is spared.
            dup(
                "backups",
                "copy-b",
                Some(Origin::Replicated),
                Some(Origin::Replicated),
                Some("k1bbb"),
                Some("k1-other"),
            ),
            dup(
                "media",
                "disc-b",
                Some(Origin::Discovered),
                Some(Origin::Discovered),
                Some("k1bbb"),
                None,
            ),
            // A discovered row with no replicated counterpart: spared.
            dup(
                "media",
                "disc-c",
                Some(Origin::Discovered),
                Some(Origin::Discovered),
                Some("k1ccc"),
                None,
            ),
        ];
        assert_eq!(
            discovered_duplicate_names(&rows),
            vec![("media".to_string(), "disc-a".to_string())]
        );
    }

    #[test]
    fn duplicate_reap_never_touches_unconfirmed_or_foreign_origins() {
        let anchor = dup(
            "backups",
            "copy-a",
            Some(Origin::Replicated),
            Some(Origin::Replicated),
            Some("k1aaa"),
            Some("k1aaa"),
        );
        // A DISCOVERED label whose own status has not landed yet (mid-write):
        // conservatively spared this pass.
        let mid_write = dup(
            "media",
            "disc-mid",
            Some(Origin::Discovered),
            None,
            Some("k1aaa"),
            None,
        );
        // A produced row wearing a forged `discovered` LABEL: its status
        // provenance disagrees, so it is never deleted (labels are forgeable).
        let forged = dup(
            "media",
            "forged",
            Some(Origin::Discovered),
            Some(Origin::Scheduled),
            Some("k1aaa"),
            Some("k1aaa"),
        );
        // An origin this build cannot parse: untouched.
        let unknown = dup("media", "weird", None, None, Some("k1aaa"), None);
        assert!(
            discovered_duplicate_names(&[anchor, mid_write, forged, unknown]).is_empty(),
            "only rows confirmed discovered on BOTH label and status may be reaped"
        );
    }

    // --- consts -----------------------------------------------------------------

    #[test]
    fn snapshot_replication_job_labels_are_group_prefixed() {
        // Group-prefixed wire strings, mirroring the api-side prefix test:
        // a typo'd prefix silently breaks the single-flight selector.
        for s in [
            SNAPSHOT_REPLICATION_INSTANCE_LABEL,
            SNAPSHOT_REPLICATION_SLOT_ANNOTATION,
        ] {
            assert!(
                s.starts_with("kopiur.home-operations.com/"),
                "{s} must be group-prefixed"
            );
        }
        // The instance label IS the copy-CR label — one label, one meaning.
        assert_eq!(
            SNAPSHOT_REPLICATION_INSTANCE_LABEL,
            kopiur_api::consts::SNAPSHOT_REPLICATION_LABEL
        );
        assert_eq!(SNAPSHOT_REPLICATION_COMPONENT, "snapshot-replication");
    }
}
