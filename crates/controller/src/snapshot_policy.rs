//! The `SnapshotPolicy` reconciler — the *recipe* (ADR §4.4).
//!
//! Responsibilities:
//! 1. Defensive re-validation via `api::validate`.
//! 2. Resolve identity via `api::identity` and pin it to `status.resolved`.
//! 3. Enforce GFS retention by calling `api::retention::select_kept` over the
//!    matching `Snapshot` CRs and deleting those outside the kept set (deletion
//!    goes through the `Snapshot` finalizer path, never a raw snapshot delete).
//!
//! The retention selection is reused verbatim from `api::retention` — this
//! module only adapts `Snapshot` CRs to its `SnapshotLike` trait and decides which
//! CRs to delete, both of which are pure and unit-tested here.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::runtime::controller::Action;
use kube::{Api, ResourceExt};

use kopiur_api::common::{PolicyDeletePolicy, RepositoryRef, Retention};
use kopiur_api::retention::{SnapshotLike, select_kept};
use kopiur_api::snapshot::PrunedBy;
use kopiur_api::{Origin, Snapshot, SnapshotPolicy, validate};

use crate::consts::{CONFIG_LABEL, POLICY_CLEANUP_FINALIZER};
use crate::context::Context;
use crate::error::{Error, Result, error_policy_for};
use crate::io;
use crate::metrics::PolicyCascadeMode;

/// A minimal view of a `Snapshot` for retention selection: its CR name (the id
/// used in delete decisions) and its snapshot end time (the GFS bucketing key).
/// `Clone` so the adoption retention gate (adoption inv. 8) can union these
/// views with candidate views without re-deriving them from the CRs.
#[derive(Debug, Clone)]
pub struct SnapshotRetentionView {
    /// CR name — the stable id returned in the kept/delete sets.
    pub name: String,
    /// Snapshot completion time (from `status.snapshot`/`status.timing`).
    pub end_time: DateTime<Utc>,
    /// Whether the `Snapshot` is pinned (`spec.pin`, ADR-0005 §13(c)) — exempt from
    /// GFS retention (never selected for deletion).
    pub pinned: bool,
}

impl SnapshotLike for SnapshotRetentionView {
    fn end_time(&self) -> DateTime<Utc> {
        self.end_time
    }
    fn id(&self) -> &str {
        &self.name
    }
    fn pinned(&self) -> bool {
        self.pinned
    }
}

/// Build a retention view from a `Snapshot` CR, using `status.timing.endTime`
/// (falling back to the CR creation timestamp). Returns `None` if the backup is
/// not in a terminal successful state — only successful snapshots participate in
/// GFS (failures are bounded separately by `failedJobsHistoryLimit`).
pub fn retention_view(b: &Snapshot) -> Option<SnapshotRetentionView> {
    use kopiur_api::SnapshotPhase;
    let status = b.status.as_ref()?;
    // Exhaustive, not `!= Succeeded`: GFS membership is a CLASSIFICATION whose
    // "no" side spans four unrelated meanings (in-flight, failed, deduped,
    // foreign). A new phase silently defaulting to "not retention-governed"
    // would quietly stop protecting a real restore point, so the compiler asks
    // here first. Deliberately NOT `is_terminal()`: `Discovered`/`Unchanged`
    // are terminal but must not claim a GFS bucket.
    let participates_in_gfs = status.phase.as_ref().is_some_and(|p| match p {
        SnapshotPhase::Succeeded => true,
        // `Unchanged` owns no manifest, so it must never displace one that
        // exists; `Discovered` is bounded by the catalog, not by this policy's
        // retention; the rest are not terminal successes at all.
        SnapshotPhase::Unchanged
        | SnapshotPhase::Discovered
        | SnapshotPhase::Pending
        | SnapshotPhase::Running
        | SnapshotPhase::Failed
        | SnapshotPhase::Deleting => false,
        // Never let a phase this build cannot read enter a set whose losers get
        // DELETED from the repository.
        SnapshotPhase::Unknown(_) => false,
    });
    if !participates_in_gfs {
        return None;
    }
    // PROVENANCE (defense in depth): a `Succeeded` row only participates in GFS
    // when it carries CONTROLLER-WRITTEN provenance (`status.snapshot`, the kopia
    // id the operator produced or adopted). This closes the phantom-Succeeded
    // displacement even if a phase were ever pinned without one — a forged bare
    // `origin: adopted` label whose creationTimestamp fallback would otherwise
    // claim a GFS bucket and displace a real snapshot into the retention delete
    // set. Every genuine produced/adopted row has `status.snapshot`.
    status.snapshot.as_ref()?;
    let end_time = status
        .timing
        .as_ref()
        .and_then(|t| t.end_time.as_deref())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            // metadata.creationTimestamp is a k8s-openapi `Time` wrapping a
            // jiff `Timestamp`; convert via unix seconds to chrono.
            b.creation_timestamp()
                .and_then(|t| DateTime::<Utc>::from_timestamp(t.0.as_second(), 0))
        })?;
    Some(SnapshotRetentionView {
        name: b.name_any(),
        end_time,
        // A pinned Snapshot is exempt from GFS pruning (ADR-0005 §13(c)).
        pinned: b.spec.pin,
    })
}

/// Decide which `Snapshot` CR names to delete under a GFS `policy`. Wraps
/// `api::retention::select_kept`; returns the `delete` set. Snapshots that are not
/// terminal-successful are ignored entirely (never selected for deletion here).
/// `policy_is_multi` is [`kopiur_api::is_multi_repo`] over the CURRENT spec —
/// see [`retention_group_key`] for how it shapes the buckets.
pub fn backups_to_delete(
    backups: &[Snapshot],
    policy: &Retention,
    policy_is_multi: bool,
) -> Vec<String> {
    // GFS is applied PER SOURCE, not per policy. `select_kept` has no grouping
    // key, so feeding one policy's whole child set through it once would make a
    // 7-PVC `pvcSelector` fan-out under `keepDaily: 7` keep SEVEN SNAPSHOTS
    // TOTAL — one day across all seven volumes — instead of seven days each.
    // That is silent data loss introduced by the fan-out, so the grouping is
    // not optional (#346).
    //
    // Un-fanned Snapshots all share the empty key, so a single-source policy is
    // exactly one bucket and behaves byte-for-byte as before.
    let mut buckets: BTreeMap<String, Vec<SnapshotRetentionView>> = BTreeMap::new();
    for b in backups {
        if let Some(v) = retention_view(b) {
            buckets
                .entry(retention_group_key(b, policy_is_multi))
                .or_default()
                .push(v);
        }
    }
    buckets
        .values()
        .flat_map(|views| select_kept(views, policy).delete)
        .collect()
}

/// **Pure.** The retention bucket a `Snapshot` belongs to.
///
/// One bucket per distinct backup source, so GFS keeps `keepDaily` days *of
/// each PVC* rather than `keepDaily` snapshots across all of them. Empty for an
/// un-fanned Snapshot, which is what makes a single-source policy one bucket
/// and therefore unchanged.
///
/// **Multi-repo fan-out (#368):** while the policy is CURRENTLY multi-repo
/// (`policy_is_multi`), the key also carries the child's mint-time repository
/// pin (`spec.repository`, normalized at mint), so GFS keeps `keepDaily` days
/// per (source, repository) — the N repositories are independent captures and
/// must retain independently. The repo component comes from the SPEC pin ONLY
/// (never `status.resolved` — a status-derived key would flap with backfills),
/// and applies ONLY while the policy is multi-repo:
///
/// - single-repo policy (including after a multi→single edit): source-only
///   buckets, byte-identical to today. Old pinned children merge back into the
///   flat buckets — a documented TRANSIENT GFS mixing (the surviving repo's
///   rows and the removed repo's leftovers compete in one bucket) that
///   self-resolves as the removed repo's rows age out of every keep window.
/// - multi-repo policy: (source, pin) buckets. Rows with NO pin (pre-feature
///   children minted before the single→multi edit) land in the ""-repo bucket
///   and age out; the policy reconciler's spec-pin backfill
///   ([`repository_pin_backfill_patches`]) converges them into their real
///   buckets first, so the ""-bucket is a shrinking transition set, not a
///   steady state.
pub fn retention_group_key(b: &Snapshot, policy_is_multi: bool) -> String {
    let source = match b.spec.source.as_ref().map(|s| &s.target) {
        Some(kopiur_api::SnapshotSourceTarget::Pvc(t)) => {
            format!("pvc/{}/{}", t.namespace, t.name)
        }
        None => String::new(),
    };
    if !policy_is_multi {
        return source;
    }
    let repo = b
        .spec
        .repository
        .as_ref()
        .map(|r| kopiur_api::common::repo_key(r, b.namespace().as_deref().unwrap_or_default()))
        .unwrap_or_default();
    // '\n' can appear in neither component (DNS names / repo keys), so the
    // joined key is injective over (source, repo).
    format!("{source}\n{repo}")
}

/// **Pure.** The `Unchanged` Snapshots to prune: everything past the newest
/// `limit`, newest first.
///
/// These rows are bounded by NOTHING else. GFS retention skips them —
/// [`retention_view`] requires `Succeeded` *and* a recorded `status.snapshot`,
/// and an `Unchanged` run has neither, which is correct: a restore point that
/// does not exist must never displace one that does. But
/// `failedJobsHistoryLimit` only counts `Failed`, so without this an hourly
/// schedule over a static PVC accrues 24 `Unchanged` CRs a day, forever (#351).
///
/// They carry no kopia manifest, so deleting one reclaims nothing and risks
/// nothing; their only value is the observability of "the schedule ran and
/// found nothing to do". Bounded by the flat `failedJobsHistoryLimit` default
/// rather than a new knob, and applied here — over the policy's `CONFIG_LABEL`
/// children — so manual `kubectl kopiur snapshot now` runs are bounded too, not
/// just scheduled ones.
pub fn unchanged_snapshots_to_prune(
    backups: &[Snapshot],
    limit: u32,
    policy_is_multi: bool,
) -> Vec<String> {
    use kopiur_api::SnapshotPhase;
    // Bucketed by source (and, for a multi-repo policy, the repository pin),
    // exactly like `backups_to_delete` above and for the same reason: a flat
    // bound over a 20-PVC fan-out would keep 10 rows TOTAL, so most volumes
    // would retain no record that the schedule ran at all, and every pass
    // would churn ~10 finalizer-guarded deletes.
    let mut buckets: BTreeMap<String, Vec<&Snapshot>> = BTreeMap::new();
    for s in backups.iter().filter(|s| {
        s.status.as_ref().and_then(|st| st.phase.as_ref()) == Some(&SnapshotPhase::Unchanged)
            && s.metadata.deletion_timestamp.is_none()
    }) {
        buckets
            .entry(retention_group_key(s, policy_is_multi))
            .or_default()
            .push(s);
    }
    buckets
        .into_values()
        .flat_map(|mut rows| {
            // Newest first; an unknown terminal time sorts last (oldest) →
            // pruned first.
            rows.sort_by_key(|s| std::cmp::Reverse(snapshot_end_or_creation(s)));
            rows.into_iter()
                .skip(limit as usize)
                .filter_map(|s| s.metadata.name.clone())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Upper bound on spec-pin backfill patches issued per reconcile pass — keeps
/// a thousand-child policy's single→multi edit from turning one reconcile into
/// a thousand-write burst; the steady requeue drains the rest.
const PIN_BACKFILL_BATCH: usize = 50;

/// **Pure.** The single→multi edit backfill decision (#368): which existing
/// produced children of a NOW-multi-repo policy should get `spec.repository`
/// patched on, and with what. Rows in → `(name, spec merge-patch body)` out;
/// the reconciler executes them (bounded by [`PIN_BACKFILL_BATCH`]).
///
/// A row is selected iff it has a `policyRef` (produced/adopted — discovered
/// rows have no recipe), NO `spec.repository` pin yet, IS NOT terminating, and
/// its `status.resolved.repository` names the repository it actually ran
/// against — that run-time pin is controller-written truth, so promoting it to
/// the spec pin is a record of fact, not a guess. Rows with NEITHER pin stay
/// unpinned (the ""-repo retention bucket) and age out — their repository is
/// genuinely unknowable and is never invented. Idempotent: once patched, the
/// spec pin exists and the row is never selected again.
///
/// The patch body is a JSON merge over `spec` carrying only `repository`
/// (already normalized — the run-time pin is written normalized), applied via
/// SSA under a dedicated field manager so this backfill owns exactly that one
/// field and can never shed or clobber the minting manager's other spec fields.
pub fn repository_pin_backfill_patches(backups: &[Snapshot]) -> Vec<(String, serde_json::Value)> {
    backups
        .iter()
        .filter(|b| {
            b.spec.policy_ref.is_some()
                && b.spec.repository.is_none()
                && b.metadata.deletion_timestamp.is_none()
        })
        .filter_map(|b| {
            let pinned = b.status.as_ref()?.resolved.as_ref()?.repository.as_ref()?;
            Some((
                b.metadata.name.clone()?,
                serde_json::json!({ "spec": { "repository": pinned } }),
            ))
        })
        .take(PIN_BACKFILL_BATCH)
        .collect()
}

/// **Pure.** Restrict a prune-execution set to rows the reconciler can act on
/// while part of a multi-repo policy's fleet is down (#368 M10).
///
/// `all_ready` short-circuits to the full set — which is every single-repo
/// pass that reaches pruning at all (a not-ready single repo parks earlier),
/// so the classic shape is byte-identical. With a not-ready subset, only rows
/// whose mint-time pin (`spec.repository`) names a READY repository execute: a
/// delete fires the Snapshot finalizer, which contacts the row's repository —
/// against a down repo that just parks the row in `Deleting`. Unpinned rows
/// (pre-fan-out history whose repository is not knowable from the spec) are
/// deferred too, conservatively. Deferred rows are simply re-selected by the
/// next pass once their repository recovers — GFS selection is deterministic.
fn executable_prunes(
    selected: Vec<String>,
    backups: &[Snapshot],
    ready_keys: &std::collections::BTreeSet<String>,
    all_ready: bool,
) -> Vec<String> {
    if all_ready {
        return selected;
    }
    use std::collections::HashMap;
    let by_name: HashMap<&str, &Snapshot> = backups
        .iter()
        .filter_map(|b| b.metadata.name.as_deref().map(|n| (n, b)))
        .collect();
    selected
        .into_iter()
        .filter(|name| {
            by_name.get(name.as_str()).is_some_and(|b| {
                b.spec.repository.as_ref().is_some_and(|pin| {
                    let ns = b.namespace().unwrap_or_default();
                    ready_keys.contains(&kopiur_api::common::repo_key(pin, &ns))
                })
            })
        })
        .collect()
}

/// **Pure.** The actionable message for the `RepositoriesReady=False` gate,
/// naming every not-ready repository. Deterministic (spec order) so the guarded
/// status write stays a no-op while the outage persists.
fn policy_repo_gate_message(not_ready_keys: &[String]) -> String {
    format!(
        "repository(ies) not Ready: {} — backups, retention, adoption and verification \
         against them are deferred until they recover (the ready subset keeps processing)",
        not_ready_keys.join(", ")
    )
}

/// **Pure.** The `status.repositorySummary` print-column value (#368 B1): the
/// comma-joined repository names — the one name for the single-repo shape —
/// capped near a kubectl column width with a ` +N` overflow marker.
pub fn repository_summary_string(names: &[&str]) -> String {
    const CAP: usize = 63;
    let mut out = String::new();
    let mut included = 0usize;
    for (i, n) in names.iter().enumerate() {
        let candidate = if out.is_empty() {
            (*n).to_string()
        } else {
            format!("{out}, {n}")
        };
        let remaining_after = names.len() - i - 1;
        let suffix = if remaining_after > 0 {
            format!(" +{remaining_after}").len()
        } else {
            0
        };
        // The first name always renders (a DNS-1123 name fits the cap alone).
        if i > 0 && candidate.len() + suffix > CAP {
            break;
        }
        out = candidate;
        included = i + 1;
    }
    let more = names.len() - included;
    if more > 0 {
        format!("{out} +{more}")
    } else {
        out
    }
}

/// **Pure.** Whether THIS repository holds a verifiable snapshot from this
/// policy — the per-repo #168 verification-gate input for the multi-repo
/// shape: a retention-visible (`Succeeded` + provenance) row whose mint-time
/// pin names the repository. Unpinned successes (pre-fan-out history) count
/// for NO repo — their repository is not knowable from the spec, and unlocking
/// a repo with no snapshot would only mint a mover Job that fails "no snapshot
/// to verify"; the spec-pin backfill converges old rows onto pins quickly, and
/// the discovered-row probe covers adopted repositories meanwhile.
fn multi_repo_has_success(backups: &[Snapshot], repo_key: &str) -> bool {
    backups.iter().any(|b| {
        retention_view(b).is_some()
            && b.spec.repository.as_ref().is_some_and(|pin| {
                let ns = b.namespace().unwrap_or_default();
                kopiur_api::common::repo_key(pin, &ns) == repo_key
            })
    })
}

/// The terminal ordering key for a Snapshot: `status.timing.endTime`, falling
/// back to `metadata.creationTimestamp`.
fn snapshot_end_or_creation(b: &Snapshot) -> Option<DateTime<Utc>> {
    b.status
        .as_ref()
        .and_then(|s| s.timing.as_ref())
        .and_then(|t| t.end_time.as_deref())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            b.creation_timestamp()
                .and_then(|t| DateTime::<Utc>::from_timestamp(t.0.as_second(), 0))
        })
}

/// **Pure.** Split the GFS-`selected` deletion set against the LIVE objects into
/// `(to_delete, to_stamp_only)`:
///
/// - `to_delete`: selected AND NOT terminating — the normal prune: stamp
///   `pruned-by: retention` then delete.
/// - `to_stamp_only`: selected AND terminating AND MISSING a valid `pruned-by`
///   annotation — an old-operator prune wave that was `kubectl delete`d before
///   this code stamped a discriminator. Stamp the annotation ONLY (no delete —
///   it is already terminating) so its finalizer reclassifies as a prune and
///   DRAINS without a human ack, instead of being held as an external deletion.
/// - A selected + terminating CR that ALREADY carries a valid `pruned-by` is in
///   NEITHER set (nothing to do — it is already draining correctly).
///
/// A selected name absent from `backups` (already gone) is skipped.
pub fn partition_retention_prune(
    backups: &[Snapshot],
    selected: &[String],
) -> (Vec<String>, Vec<String>) {
    use std::collections::HashMap;
    let by_name: HashMap<&str, &Snapshot> = backups
        .iter()
        .filter_map(|b| b.metadata.name.as_deref().map(|n| (n, b)))
        .collect();
    let mut to_delete = Vec::new();
    let mut to_stamp_only = Vec::new();
    for name in selected {
        let Some(b) = by_name.get(name.as_str()) else {
            continue;
        };
        if b.metadata.deletion_timestamp.is_none() {
            to_delete.push(name.clone());
        } else if crate::snapshot::pruned_by(b.annotations()).is_none() {
            to_stamp_only.push(name.clone());
        }
    }
    (to_delete, to_stamp_only)
}

/// The plan for cascading a `SnapshotPolicy` deletion onto the `Snapshot` CRs
/// carrying its config label, executed by [`handle_policy_deletion`] (the
/// [`POLICY_CLEANUP_FINALIZER`](crate::consts::POLICY_CLEANUP_FINALIZER) body).
/// **Pure** — no IO, no repository contact of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyCascadePlan {
    /// `PolicyDeletePolicy::Retain`, live children: stamp `pruned-by:
    /// policy-cascade` then delete the CR (the kopia snapshot is NEVER
    /// touched — the stamp is what routes the Snapshot finalizer to
    /// [`crate::snapshot::DeletionPlan::RetainSnapshotOnPolicyDelete`] instead
    /// of a real repository delete, even when the Snapshot's own
    /// `deletionPolicy` is `Delete`).
    pub stamp_and_delete: Vec<String>,
    /// `PolicyDeletePolicy::Retain`, terminating children with NO valid
    /// `pruned-by` stamp: an in-flight external (or breaker-held) deletion,
    /// reclassified into a quiet retain drain by stamping the annotation ONLY
    /// (already terminating — there is nothing left to delete). This is the
    /// finalizer-release guarantee: a breaker-held terminating child must
    /// resolve to "no work" here, or the M3 policy finalizer would wedge
    /// forever waiting on a mass-deletion ack that will never come once the
    /// policy itself is gone.
    pub stamp_only: Vec<String>,
    /// `PolicyDeletePolicy::Delete`, live children: a bare UNSTAMPED delete —
    /// each child's own `deletionPolicy` applies as an ordinary EXTERNAL
    /// deletion, subject to the per-repository mass-deletion breaker. NEVER
    /// stamped: stamping here would launder an external-classified deletion
    /// past the breaker.
    pub delete_only: Vec<String>,
}

/// **Pure.** Decide how a `SnapshotPolicy` deletion cascades onto its
/// `children` `Snapshot` CRs under the policy's effective `mode`
/// ([`kopiur_api::snapshot_policy::effective_on_policy_delete`]).
///
/// Rules, in order:
/// 1. **Origin filter** (exhaustive over [`Origin`], status-first via
///    [`crate::snapshot::resolve_origin`]): `Discovered` is NEVER included in
///    any set — a hand-labeled discovered CR must not churn just because a
///    policy it merely resembles was deleted. `Replicated` and
///    unparseable-origin rows are excluded too (see [`cascade_eligible`]).
///    `Adopted | Scheduled | Manual` are cascaded — all three are
///    operator-managed rows.
/// 2. **Terminating exclusion**: only a child with
///    `metadata.deletionTimestamp.is_none()` (live) may enter
///    `stamp_and_delete` or `delete_only`. A terminating child is handled by
///    `stamp_only` (Retain mode, unstamped) or left alone entirely (every
///    other case) — never re-deleted.
/// 3. **Exhaustive match on `(terminating, mode)`** (2×2, no catch-all):
///    - `(live, Retain)` → `stamp_and_delete`.
///    - `(live, Delete)` → `delete_only`.
///    - `(terminating, Retain)` → `stamp_only` iff no valid `pruned-by` stamp
///      is already present (an already-stamped terminating child is already
///      draining correctly — nothing to do).
///    - `(terminating, Delete)` → nothing (already terminating; `Delete` mode
///      never stamps).
pub fn plan_policy_cascade(children: &[Snapshot], mode: PolicyDeletePolicy) -> PolicyCascadePlan {
    let mut plan = PolicyCascadePlan::default();
    for child in children {
        if cascade_eligible(child) {
            classify_policy_cascade_child(child, mode, &mut plan);
        }
    }
    plan
}

/// Step 1 of [`plan_policy_cascade`]: exhaustive over [`Origin`].
/// `Discovered` is excluded — the operator did not create that kopia snapshot
/// and must never churn it on a policy's say-so. `Replicated` is excluded for
/// the sibling reason: a dest-side copy CR belongs to its
/// `SnapshotReplication` (it carries no `policyRef` and never earns the config
/// label), so no `SnapshotPolicy` deletion may cascade onto it. An
/// unparseable origin marker (`None`) is excluded conservatively — never
/// churn a row this build cannot classify.
fn cascade_eligible(child: &Snapshot) -> bool {
    match crate::snapshot::resolve_origin(child) {
        Some(Origin::Discovered | Origin::Replicated) | None => false,
        Some(Origin::Adopted | Origin::Scheduled | Origin::Manual) => true,
    }
}

/// Steps 2-3 of [`plan_policy_cascade`]: exhaustive over `(terminating,
/// mode)` for one already-eligible child.
fn classify_policy_cascade_child(
    child: &Snapshot,
    mode: PolicyDeletePolicy,
    plan: &mut PolicyCascadePlan,
) {
    let name = child.name_any();
    let terminating = child.metadata.deletion_timestamp.is_some();
    match (terminating, mode) {
        (false, PolicyDeletePolicy::Retain) => plan.stamp_and_delete.push(name),
        (false, PolicyDeletePolicy::Delete) => plan.delete_only.push(name),
        (true, PolicyDeletePolicy::Retain) => {
            if crate::snapshot::pruned_by(child.annotations()).is_none() {
                plan.stamp_only.push(name);
            }
        }
        (true, PolicyDeletePolicy::Delete) => {}
    }
}

/// Cap on combined cascade actions ([`PolicyCascadePlan::stamp_and_delete`] +
/// [`PolicyCascadePlan::stamp_only`] + [`PolicyCascadePlan::delete_only`])
/// executed by [`handle_policy_deletion`] in a single reconcile pass, so a
/// policy with a very large child population can't balloon one reconcile's
/// IO. Each pass re-LISTs and re-plans, so remaining work is simply picked up
/// on the next pass.
const POLICY_CASCADE_BATCH: usize = 50;

/// One pass's execution slice of a [`PolicyCascadePlan`]: which prefix of each
/// set to act on, in preference order (`stamp_and_delete`, then `stamp_only`,
/// then `delete_only`), capped at `cap` combined actions. **Pure** — split out
/// so the batching/ordering itself is unit-tested without a cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PolicyCascadeBatch<'a> {
    stamp_and_delete: &'a [String],
    stamp_only: &'a [String],
    delete_only: &'a [String],
}

/// **Pure.** See [`PolicyCascadeBatch`].
fn slice_policy_cascade_batch(plan: &PolicyCascadePlan, cap: usize) -> PolicyCascadeBatch<'_> {
    let sd_take = plan.stamp_and_delete.len().min(cap);
    let so_take = plan.stamp_only.len().min(cap - sd_take);
    let do_take = plan.delete_only.len().min(cap - sd_take - so_take);
    PolicyCascadeBatch {
        stamp_and_delete: &plan.stamp_and_delete[..sd_take],
        stamp_only: &plan.stamp_only[..so_take],
        delete_only: &plan.delete_only[..do_take],
    }
}

/// Thin IO: execute one pass's [`slice_policy_cascade_batch`] over `plan`
/// (capped at [`POLICY_CASCADE_BATCH`]). Every step is idempotent and
/// 404-tolerant, so a crash mid-pass simply re-runs safely on the next
/// reconcile. Each set's action is its own tiny helper (complexity ratchet:
/// keeps this orchestrator a flat sequence of calls).
async fn execute_policy_cascade_pass(
    ctx: &Context,
    backup_api: &Api<Snapshot>,
    namespace: &str,
    plan: &PolicyCascadePlan,
) -> Result<()> {
    let batch = slice_policy_cascade_batch(plan, POLICY_CASCADE_BATCH);
    execute_stamp_and_delete(ctx, backup_api, namespace, batch.stamp_and_delete).await?;
    execute_stamp_only(backup_api, namespace, batch.stamp_only).await?;
    execute_delete_only(ctx, backup_api, namespace, batch.delete_only).await?;
    Ok(())
}

/// `PolicyCascadeBatch::stamp_and_delete` half of [`execute_policy_cascade_pass`]:
/// stamp `pruned-by: policy-cascade` then delete (Retain mode).
async fn execute_stamp_and_delete(
    ctx: &Context,
    backup_api: &Api<Snapshot>,
    namespace: &str,
    names: &[String],
) -> Result<()> {
    for cr_name in names {
        io::annotate_then_delete_snapshot(backup_api, cr_name, PrunedBy::PolicyCascade).await?;
        ctx.metrics
            .inc_policy_cascade_children_deleted(namespace, PolicyCascadeMode::Retain);
        tracing::info!(namespace, snapshot = %cr_name, "policy cascade: stamped pruned-by then deleted (Retain mode)");
    }
    Ok(())
}

/// `PolicyCascadeBatch::stamp_only` half of [`execute_policy_cascade_pass`]:
/// reclassify an in-flight terminating child (no delete — it is already
/// terminating). Not counted by `kopiur_policy_cascade_children_deleted` (see
/// its description) since nothing is deleted here.
async fn execute_stamp_only(
    backup_api: &Api<Snapshot>,
    namespace: &str,
    names: &[String],
) -> Result<()> {
    for cr_name in names {
        io::stamp_pruned_by(backup_api, cr_name, PrunedBy::PolicyCascade).await?;
        tracing::info!(namespace, snapshot = %cr_name, "policy cascade: reclassified an in-flight terminating child (stamp only)");
    }
    Ok(())
}

/// `PolicyCascadeBatch::delete_only` half of [`execute_policy_cascade_pass`]:
/// a bare unstamped delete (Delete mode, external classification).
async fn execute_delete_only(
    ctx: &Context,
    backup_api: &Api<Snapshot>,
    namespace: &str,
    names: &[String],
) -> Result<()> {
    for cr_name in names {
        io::delete_snapshot(backup_api, cr_name).await?;
        ctx.metrics
            .inc_policy_cascade_children_deleted(namespace, PolicyCascadeMode::Delete);
        tracing::info!(namespace, snapshot = %cr_name, "policy cascade: bare deleted (Delete mode, external classification)");
    }
    Ok(())
}

/// Drive the `SnapshotPolicy` deletion-cascade finalizer body: LIST this
/// policy's `Snapshot` children (by [`CONFIG_LABEL`]), plan the cascade via
/// [`plan_policy_cascade`], execute one pass ([`execute_policy_cascade_pass`]),
/// and release the finalizer once a pass's plan is **entirely empty** (no
/// `stamp_and_delete`, `stamp_only`, or `delete_only` work at all).
///
/// A plan whose ONLY work is `stamp_only` still executes-then-requeues rather
/// than releasing in the same pass: the next pass re-LISTs, observes those
/// children now stamped (so `classify_policy_cascade_child`'s `(terminating,
/// Retain)` arm no longer selects them — a valid `pruned-by` stamp is already
/// present), and its plan is then genuinely empty, releasing at that point.
/// This costs at most one extra ~2s pass and is what keeps a breaker-held
/// terminating child from EVER wedging finalizer removal — M2's planner
/// already guarantees such a child produces no `stamp_and_delete`/`delete_only`
/// work, so this never waits on a mass-deletion ack that will never come once
/// the policy itself is gone.
async fn handle_policy_deletion(
    ctx: &Context,
    api: &Api<SnapshotPolicy>,
    config: &SnapshotPolicy,
    namespace: &str,
    name: &str,
) -> Result<Action> {
    // Nothing to clean up if our finalizer isn't present (e.g. a CR created
    // and deleted before this version's finalizer was ever stamped).
    if !config
        .finalizers()
        .iter()
        .any(|f| f == POLICY_CLEANUP_FINALIZER)
    {
        return Ok(Action::await_change());
    }

    let backup_api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), namespace);
    let lp = ListParams::default().labels(&format!("{CONFIG_LABEL}={name}"));
    let children = backup_api.list(&lp).await?.items;
    let mode =
        kopiur_api::snapshot_policy::effective_on_policy_delete(config.spec.deletion.as_ref());
    let plan = plan_policy_cascade(&children, mode);

    if plan.stamp_and_delete.is_empty() && plan.stamp_only.is_empty() && plan.delete_only.is_empty()
    {
        io::remove_finalizer(api, config, POLICY_CLEANUP_FINALIZER).await?;
        tracing::info!(policy = %name, ?mode, "policy deletion cascade complete; finalizer released");
        return Ok(Action::await_change());
    }

    tracing::info!(
        policy = %name,
        ?mode,
        stamp_and_delete = plan.stamp_and_delete.len(),
        stamp_only = plan.stamp_only.len(),
        delete_only = plan.delete_only.len(),
        "policy deletion cascade: executing pass"
    );
    execute_policy_cascade_pass(ctx, &backup_api, namespace, &plan).await?;
    Ok(Action::requeue(std::time::Duration::from_secs(2)))
}

/// Count the most-recent run of consecutive `Failed` backups before the latest
/// `Succeeded` one (the store-backed `kopiur_snapshot_consecutive_failures`
/// gauge, derived per policy from the Snapshot store in
/// `crate::metrics::Metrics::register_resource_observers`). Only terminal
/// backups (Succeeded/Failed) count; ordering is by `endTime` (falling back to
/// the CR creation time). Pure. ADR §4.13.
pub fn consecutive_failures<'a>(backups: impl IntoIterator<Item = &'a Snapshot>) -> i64 {
    use kopiur_api::SnapshotPhase;
    let terminal_time = |b: &Snapshot| -> Option<(DateTime<Utc>, SnapshotPhase)> {
        let status = b.status.as_ref()?;
        let phase = status.phase.clone()?;
        // Exhaustive, NOT `SnapshotPhase::is_terminal()`: a streak is about RUNS,
        // so `Discovered` (terminal, but never a run of this policy) is
        // deliberately excluded — the sets differ.
        let counts_toward_streak = match phase {
            SnapshotPhase::Succeeded | SnapshotPhase::Failed | SnapshotPhase::Unchanged => true,
            SnapshotPhase::Pending
            | SnapshotPhase::Running
            | SnapshotPhase::Deleting
            | SnapshotPhase::Discovered => false,
            // A phase this build cannot read is neither a success nor a
            // failure; keeping it out leaves the streak on known evidence only.
            SnapshotPhase::Unknown(_) => false,
        };
        if !counts_toward_streak {
            return None;
        }
        let t = status
            .timing
            .as_ref()
            .and_then(|t| t.end_time.as_deref())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .or_else(|| {
                b.creation_timestamp()
                    .and_then(|t| DateTime::<Utc>::from_timestamp(t.0.as_second(), 0))
            })?;
        Some((t, phase))
    };
    let mut terminal: Vec<(DateTime<Utc>, SnapshotPhase)> =
        backups.into_iter().filter_map(terminal_time).collect();
    // Newest first.
    terminal.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    let mut n = 0;
    for (_, phase) in terminal {
        match phase {
            SnapshotPhase::Failed => n += 1,
            // `Unchanged` breaks the streak exactly like `Succeeded`: a run that
            // read the source and found it identical is proof the backup path
            // works. Letting it fall through to a `_ => {}` wildcard would leave
            // the streak frozen at its last value, so `KopiurBackupFailing`
            // could never clear for a policy that recovered and then went quiet.
            SnapshotPhase::Succeeded | SnapshotPhase::Unchanged => break,
            // Unreachable: `terminal_time` already filtered these out. Named
            // rather than `_ =>` so the filter and this match must be updated
            // together when a phase is added.
            SnapshotPhase::Pending
            | SnapshotPhase::Running
            | SnapshotPhase::Deleting
            | SnapshotPhase::Discovered
            | SnapshotPhase::Unknown(_) => {}
        }
    }
    n
}

/// Parse an RFC3339 timestamp (e.g. `status.lastVerified`) to Unix seconds for
/// the `kopiur_snapshot_verified_timestamp_seconds` gauge. `None` on a malformed
/// value so a bad timestamp simply leaves the gauge unset rather than crashing.
///
/// Shared with the Restore reconciler, which re-derives
/// `kopiur_restore_duration_seconds` from its pinned status timing the same way.
pub(crate) fn rfc3339_unix_secs(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Reconcile a `SnapshotPolicy`.
#[tracing::instrument(skip(config, ctx), fields(kind = "SnapshotPolicy", namespace = %config.namespace().unwrap_or_default(), name = %config.name_any()))]
pub async fn reconcile(config: Arc<SnapshotPolicy>, ctx: Arc<Context>) -> Result<Action> {
    let start = std::time::Instant::now();
    let result = reconcile_inner(&config, &ctx).await;
    ctx.metrics
        .record_reconcile("SnapshotPolicy", start.elapsed().as_secs_f64());
    result
}

/// `reconcile_inner`'s deletion/finalizer front-matter, extracted to a single
/// call (complexity ratchet: `reconcile_inner` only gains calls). `Some(action)`
/// means the caller must return it immediately without reconciling further;
/// `None` means the policy is live and finalized — proceed as normal.
async fn handle_policy_lifecycle(
    ctx: &Context,
    api: &Api<SnapshotPolicy>,
    config: &SnapshotPolicy,
    namespace: &str,
    name: &str,
) -> Result<Option<Action>> {
    if config.metadata.deletion_timestamp.is_some() {
        return Ok(Some(
            handle_policy_deletion(ctx, api, config, namespace, name).await?,
        ));
    }
    // Ensure the cleanup finalizer before anything else runs, so the deletion
    // branch above is guaranteed to observe it later.
    if io::ensure_finalizer(api, config, POLICY_CLEANUP_FINALIZER).await? {
        return Ok(Some(Action::requeue(std::time::Duration::from_secs(1))));
    }
    Ok(None)
}

async fn reconcile_inner(config: &SnapshotPolicy, ctx: &Context) -> Result<Action> {
    let errs = validate::validate_backup_config(&config.spec);
    if let Some(first) = errs.into_iter().next() {
        return Err(Error::Validation(first.to_string()));
    }
    // Every repository this recipe targets: one for the classic single-repo
    // shape, 1-8 for the multi-repository fan-out. The neither/both shapes come
    // back as the exactly-one-of validation error (defensive re-check — the
    // validator above already refused them).
    let repo_targets: Vec<&RepositoryRef> = match kopiur_api::policy_repositories(&config.spec)
        .map_err(|e| Error::Validation(e.to_string()))?
    {
        kopiur_api::PolicyRepositories::Single(r) => vec![r],
        kopiur_api::PolicyRepositories::Multi(rs) => rs.iter().collect(),
    };
    let is_multi = kopiur_api::is_multi_repo(&config.spec);

    let namespace = config
        .namespace()
        .ok_or_else(|| Error::Invariant("SnapshotPolicy has no namespace".into()))?;
    let name = config.name_any();
    let generation = config.metadata.generation;
    let api: Api<SnapshotPolicy> = Api::namespaced(ctx.client.clone(), &namespace);

    // Deletion trumps suspend: a suspended policy that is deleted still
    // cascades onto its Snapshot children, so this is checked BEFORE the
    // suspend branch below.
    if let Some(action) = handle_policy_lifecycle(ctx, &api, config, &namespace, &name).await? {
        return Ok(action);
    }

    let existing = config
        .status
        .as_ref()
        .map(|s| s.conditions.clone())
        .unwrap_or_default();
    // Every status write below is guarded against the cached status: this
    // reconciler re-runs on each referenced-repository event (the
    // `repository_to_policies` fan-out), so a steady-state pass must issue zero
    // PATCHes — both to avoid the status-churn self-trigger class and to keep a
    // repo event storm from multiplying into apiserver writes per policy.
    let current = serde_json::to_value(&config.status).ok();

    // §14(e): a suspended SnapshotPolicy is skipped entirely (no identity re-pin, no
    // retention prune). Surface `Ready=False`/`Reconciling=False` so GitOps sees a
    // deliberate pause rather than a hang, then back off long.
    if config.spec.suspend {
        let conditions = io::set_ready(
            &existing,
            generation,
            io::ReadyOutcome::Reconciling,
            "Suspended",
            "SnapshotPolicy is suspended (spec.suspend); skipping retention and backups",
        );
        io::patch_status_if_changed(
            &api,
            &name,
            current.as_ref(),
            serde_json::json!({ "observedGeneration": generation, "conditions": conditions }),
        )
        .await?;
        return Ok(Action::requeue(std::time::Duration::from_secs(300)));
    }

    // 1. Resolve each target repository and the identity UNDER that repository's
    //    CEL `identityDefaults` (the unit of identity is (repo, identity) — a
    //    multi-repo policy legitimately has N identities). This runs on EVERY
    //    reconcile, not just once at admission: it re-renders from the LIVE
    //    repository's `identityDefaults` (ADR-0004 §5) each time, so
    //    `status.resolved` mirrors the current resolution rather than freezing
    //    the first one. What keeps an already-snapshotted policy from silently
    //    re-identifying is the fork guard at admission (`IdentityWouldFork`/
    //    `RepositoryIdentityWouldFork`), not this pin.
    let mut targets: Vec<PolicyRepoTarget> = Vec::with_capacity(repo_targets.len());
    for rref in &repo_targets {
        let repo = io::resolve_repository_ref(
            &ctx.client,
            rref,
            &namespace,
            ctx.operator_namespace.as_deref(),
        )
        .await?;
        let resolved =
            resolve_config_identity(config, &namespace, repo.identity_defaults.as_ref())?;
        targets.push(PolicyRepoTarget {
            rref: (*rref).clone(),
            repo,
            resolved,
        });
    }
    // status.resolved mirror: the single-repo shape pins its one resolution,
    // exactly as before (byte-identical wire — `repositories` elides empty).
    // A multi-repo policy mirrors the per-repo `ResolvedPolicy.repositories`
    // wire (#368 M9/M10): one entry per member carrying the identity resolved
    // under THAT repository's `identityDefaults` — the admission fork guard's
    // per-repo baseline — with the top-level `identity` deliberately absent
    // rather than silently mirroring repository #1's identity as "the" identity.
    let resolved_mirror = resolved_policy_mirror(is_multi, &targets);
    io::patch_status_if_changed(
        &api,
        &name,
        current.as_ref(),
        serde_json::json!({ "resolved": resolved_mirror }),
    )
    .await?;

    // LIST this policy's Snapshots BEFORE the repository-readiness gate below:
    // retention (further down) needs the population even while the repository is
    // not Ready. The `kopiur_snapshot_consecutive_failures` / `kopiur_snapshots_live`
    // gauges are no longer written here — they are store-backed observables derived
    // from the Snapshot reflector store at collection time (M6, #345), which is
    // what freeze-proofs them: they keep updating even when this reconcile stops
    // running (e.g. the repository left `Ready`, exactly when the streak matters
    // most), and a deleted policy's series vanish instead of lingering (#172/#175).
    let backup_api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), &namespace);
    let lp = ListParams::default().labels(&format!("{CONFIG_LABEL}={name}"));
    let backups = backup_api.list(&lp).await?.items;

    // §2 dependent gating, refined to the READY SUBSET (#368 M10): partition
    // the targets by repository readiness. The ready subset keeps processing
    // (retention, adoption, verification); the not-ready subset is surfaced via
    // the registered structural gate (`RepositoriesReady=False` /
    // `RepositoryNotReady`, `kopiur_api::gates::POLICY_REPOSITORY_NOT_READY_GATE`)
    // so `kubectl kopiur doctor` sees the park (#359). With NO ready repository
    // — which is every not-ready single-repo policy — the reconciler parks
    // exactly as before (Reconciling + 15s requeue), now with the gate as the
    // one condition truth for the block.
    let mut ready: Vec<&PolicyRepoTarget> = Vec::new();
    let mut not_ready_keys: Vec<String> = Vec::new();
    for t in &targets {
        if io::repository_ready(&ctx.client, &t.rref, &namespace).await? {
            ready.push(t);
        } else {
            not_ready_keys.push(kopiur_api::common::repo_key(&t.rref, &namespace));
        }
    }
    let all_ready = not_ready_keys.is_empty();
    let gate_message = policy_repo_gate_message(&not_ready_keys);
    if ready.is_empty() {
        let conditions = io::set_ready(
            &existing,
            generation,
            io::ReadyOutcome::Reconciling,
            crate::consts::REPOSITORY_NOT_READY_REASON,
            "waiting for the referenced Repository to become Ready before reconciling",
        );
        let conditions = io::upsert_gate(
            &conditions,
            &kopiur_api::gates::POLICY_REPOSITORY_NOT_READY_GATE,
            &gate_message,
            generation,
        );
        io::patch_status_if_changed(
            &api,
            &name,
            current.as_ref(),
            serde_json::json!({ "observedGeneration": generation, "conditions": conditions }),
        )
        .await?;
        return Ok(Action::requeue(std::time::Duration::from_secs(15)));
    }
    // The normalized repo keys the ready subset covers — the execution filter
    // for repo-touching work (prunes) while part of the fleet is down.
    let ready_keys: std::collections::BTreeSet<String> = ready
        .iter()
        .map(|t| kopiur_api::common::repo_key(&t.rref, &namespace))
        .collect();

    // §3: surface the most recent successful child Snapshot timestamp (backs the
    // LAST-SNAPSHOT column + the staleness alert). Deterministic (the max endTime
    // over this policy's Succeeded Snapshots), so an unchanged value is a no-op
    // patch. Computed from the `backups` slice fetched once above (#382 M1 —
    // this was a second, byte-identical CONFIG_LABEL LIST).
    let last_successful = latest_successful_end_time(&backups);
    // Does this policy have a verifiable backup yet? Backs the #168 verification gate
    // below (captured before `last_successful` is consumed by the status patch).
    let has_successful_snapshot = last_successful.is_some();

    // 2. Enforce GFS retention over this policy's Snapshots (the Snapshot finalizer
    //    governs the kopia snapshot itself). The LIST happens above the
    //    repository-readiness gate — only the prune is conditional. `retention: None`
    //    deliberately means "never prune" (see `validate::snapshot`).
    // Bound the `Unchanged` rows FIRST and unconditionally: they accumulate
    // whether or not `spec.retention` is configured, because GFS never sees
    // them (no manifest, so no retention view). `retention: None` means "never
    // prune real restore points" — it does not mean "hoard empty ones".
    // Single→multi edit convergence (#368): promote each produced child's
    // run-time repository pin (`status.resolved.repository`) to the mint-time
    // spec pin the multi-repo retention buckets key on. Pure decision
    // ([`repository_pin_backfill_patches`]), SSA execution under a dedicated
    // field manager, bounded batch, idempotent — a no-op for every already-
    // pinned (or pin-less) row, so steady state issues zero writes.
    if is_multi {
        for (cr_name, body) in repository_pin_backfill_patches(&backups) {
            backfill_spec_repository_pin(&backup_api, &cr_name, body).await?;
            tracing::info!(config = %name, backup = %cr_name, "backfilled spec.repository pin from status.resolved (single→multi edit)");
        }
    }

    let unchanged_prunes = executable_prunes(
        unchanged_snapshots_to_prune(
            &backups,
            kopiur_api::consts::effective_failed_jobs_history_limit(None),
            is_multi,
        ),
        &backups,
        &ready_keys,
        all_ready,
    );
    for cr_name in unchanged_prunes {
        // Stamp `pruned-by` THEN delete, exactly like every other operator
        // prune, so the finalizer classifies it as ours and the mass-deletion
        // breaker stays out of the way.
        io::annotate_then_delete_snapshot(&backup_api, &cr_name, PrunedBy::FailedHistory).await?;
        tracing::info!(config = %name, backup = %cr_name, "pruned Unchanged Snapshot (history limit)");
    }

    if let Some(retention) = config.spec.retention.as_ref() {
        let selected = backups_to_delete(&backups, retention, is_multi);
        // Split the selected set: normal (live) prunes get stamp-then-delete;
        // an ALREADY-terminating selected CR that lacks a valid pruned-by
        // annotation (an old-operator prune straddling this upgrade) is
        // reclassified by stamping the annotation only, so its finalizer drains
        // as a prune rather than being held as an external mass deletion.
        let (to_delete, to_stamp_only) = partition_retention_prune(&backups, &selected);
        // Execute deletes only against the READY repo subset (#368 M10): a
        // delete fires the Snapshot finalizer, which contacts the row's
        // repository — against a down repo that parks the row in `Deleting`
        // for nothing. Deferred rows are re-selected next pass. `to_stamp_only`
        // is NOT filtered: a stamp is a pure annotation write (the row is
        // already terminating), and deferring it would hold the very
        // reclassification that keeps breaker-held drains moving.
        let to_delete = executable_prunes(to_delete, &backups, &ready_keys, all_ready);
        for cr_name in &to_delete {
            // Stamp `pruned-by: retention` THEN delete, so the finalizer bypasses
            // the mass-deletion breaker + cascade guard (this is an operator prune,
            // never an external deletion). 404-tolerant + idempotent internally.
            io::annotate_then_delete_snapshot(&backup_api, cr_name, PrunedBy::Retention).await?;
            tracing::info!(config = %name, backup = %cr_name, "pruned backup (GFS retention)");
        }
        for cr_name in &to_stamp_only {
            io::stamp_pruned_by(&backup_api, cr_name, PrunedBy::Retention).await?;
            tracing::info!(config = %name, backup = %cr_name, "reclassified an in-flight retention prune (pruned-by stamp only)");
        }
        // `active` = live snapshots that survive GFS (all selected are being
        // removed one way or another, so subtract the full selected set — the
        // same count the pre-partition code reported).
        let active = backups.len().saturating_sub(selected.len());
        // Only stamp `lastPruneAt`/`lastPruneDeleted` when a prune actually
        // happened. Writing `now()` on every reconcile made the status differ each
        // pass → resourceVersion bump → watch event → self-triggered reconcile (the
        // same hot-loop class as the repo bug). Between prunes the PRIOR values are
        // carried forward (a JSON merge would preserve them anyway), so the desired
        // object compares equal to the stored one and the guarded write is skipped.
        //
        // Built from the CRD's own `RetentionSummary` so the field names cannot
        // drift from the structural schema: this used to write the pre-rename
        // `activeBackupCount`, which the apiserver SILENTLY PRUNED (the schema
        // field is `activeSnapshotCount`) — caught by the retention e2e.
        let pruned = !to_delete.is_empty();
        let prior = config.status.as_ref().and_then(|s| s.retention.as_ref());
        let summary = kopiur_api::snapshot_policy::RetentionSummary {
            active_snapshot_count: Some(active as i64),
            last_prune_at: if pruned {
                Some(Utc::now().to_rfc3339())
            } else {
                prior.and_then(|r| r.last_prune_at.clone())
            },
            last_prune_deleted: if pruned {
                Some(to_delete.len() as i64)
            } else {
                prior.and_then(|r| r.last_prune_deleted)
            },
        };
        io::patch_status_if_changed(
            &api,
            &name,
            current.as_ref(),
            serde_json::json!({ "retention": summary }),
        )
        .await?;
    }

    // §5 (M6): auto-adopt discovered snapshots whose resolved identity matches
    // this recipe. Runs AFTER the retention block (inv. 6) and consumes a
    // SEPARATE cluster-wide LIST — the retention pass above already acted on the
    // `backups` LIST taken before adoption, so one reconcile never both adopts
    // and prunes the same rows (the newly-adopted rows are first seen by the
    // NEXT pass's retention). `Some(requeue)` after a wave / scan request.
    let adoption_requeue = run_adoption(
        ctx,
        &api,
        config,
        &namespace,
        &name,
        &ready,
        is_multi,
        &backups,
        current.as_ref(),
    )
    .await?;

    // #368 M10: fold the verify movers' entry-keyed stamps into the per-repo
    // `status.verification` Vec (multi only; the controller is the vec's single
    // writer — see `verification::fold_verification`). `None` for the classic
    // single-repo shape, whose flat `lastVerified` the mover stamps directly.
    let folded = fold_multi_verification(config, is_multi, &repo_targets, &namespace);

    // Final status: Ready when every repository is (Reconciling + the
    // registered `RepositoriesReady` gate otherwise), the observedGeneration,
    // the §3 lastSuccessfulSnapshot, the `repositorySummary` print column, and
    // the folded per-repo verification state.
    let conditions = policy_ready_conditions(&existing, generation, all_ready, &gate_message);
    // Warn-only: surface a deep-verify scratch `storageClassName` that is a silent
    // no-op (set with no effective `capacity` → an `emptyDir`, which has no
    // StorageClass). Folded into the SAME status patch as set_ready (single writer,
    // no two-writer flip-flop) and upserted in place so it self-clears (True→False)
    // when a capacity is added. The Warning Event fires only on the flip to True, so
    // a steady Ignored state doesn't re-publish every reconcile. Evaluated over
    // every READY repository for the multi-repo shape (any repo whose merged
    // scratch config is a no-op flags it).
    let conditions = match select_scratch_state(config, &ready) {
        Some(state) => {
            let was_ignored = existing
                .iter()
                .find(|c| c.type_ == crate::consts::SCRATCH_STORAGE_CLASS_IGNORED_CONDITION)
                .is_some_and(|c| c.status == "True");
            if state.ignored && !was_ignored {
                io::publish_warning_event(
                    ctx,
                    config,
                    crate::consts::SCRATCH_STORAGE_CLASS_IGNORED_REASON,
                    crate::consts::SET_SCRATCH_CAPACITY_ACTION,
                    &state.message,
                )
                .await;
            }
            io::upsert_condition(
                &conditions,
                crate::consts::SCRATCH_STORAGE_CLASS_IGNORED_CONDITION,
                state.ignored,
                if state.ignored {
                    crate::consts::SCRATCH_STORAGE_CLASS_IGNORED_REASON
                } else {
                    crate::consts::SCRATCH_STORAGE_CLASS_HONORED_REASON
                },
                &state.message,
                generation,
            )
        }
        None => conditions,
    };
    let status = final_status_body(
        config,
        generation,
        conditions,
        last_successful.as_deref(),
        &repo_targets,
        folded.as_ref(),
    )?;
    io::patch_status_if_changed(&api, &name, current.as_ref(), status).await?;

    // §4: surface the most recent successful verify as a gauge for staleness
    // alerting (mirrors kopiur_snapshot_last_success_timestamp_seconds). The mover
    // stamps `status.lastVerified` on a successful quick/deep verify (single-repo);
    // for multi the folded MIN across current repos is the honest fleet-wide age.
    // No-op until a first verify lands.
    let flat_verified = match folded.as_ref() {
        Some(f) => f.folded.flat.clone(),
        None => config.status.as_ref().and_then(|s| s.last_verified.clone()),
    };
    if let Some(ts) = flat_verified.as_deref().and_then(rfc3339_unix_secs) {
        ctx.metrics.set_snapshot_verified(&namespace, &name, ts);
    }

    // §4: first-class verification scheduling. When `spec.verification` is set, the
    // policy reconciler doubles as the verify scheduler (mirroring the Maintenance
    // kernel): it spawns per-slot quick/deep verify Jobs and tracks them — one per
    // READY repository for the multi-repo shape (#368 M10), each anchored on ITS
    // OWN per-repo lastVerified. When absent, `verify_step` is a no-op (None) and
    // the steady 300s requeue applies. Otherwise requeue on the shorter of the
    // steady cadence and the verify cadence so a due verification fires on time.
    let steady = std::time::Duration::from_secs(300);
    let verify_requeue = run_verify_steps(
        config,
        ctx,
        &namespace,
        &ready,
        folded.as_ref(),
        &backups,
        has_successful_snapshot,
    )
    .await?;
    let base = match verify_requeue {
        Some(verify_requeue) => steady.min(verify_requeue),
        None => steady,
    };
    // A not-ready subset keeps the prompt readiness-poll cadence so recovery is
    // observed quickly (same 15s the all-parked path uses).
    let base = if all_ready {
        base
    } else {
        base.min(std::time::Duration::from_secs(15))
    };
    // A fresh adoption wave / scan request pulls the next reconcile in sooner
    // (belt — the policy/repository watches usually re-trigger before this).
    let requeue = match adoption_requeue {
        Some(a) => base.min(a),
        None => base,
    };
    Ok(Action::requeue(requeue))
}

/// The multi-repo verification fold inputs + result: the CURRENT repo set as
/// `(normalized ref, key)` pairs and the folded per-repo state. `None` for the
/// single-repo shape. Extracted so `reconcile_inner` only gains a call
/// (complexity ratchet).
struct MultiVerification {
    /// `(normalized ref, normalized repo key)` per current repo, spec order.
    current_repos: Vec<(RepositoryRef, String)>,
    folded: crate::verification::FoldedVerification,
}

/// See [`MultiVerification`].
fn fold_multi_verification(
    config: &SnapshotPolicy,
    is_multi: bool,
    repo_targets: &[&RepositoryRef],
    namespace: &str,
) -> Option<MultiVerification> {
    if !is_multi {
        return None;
    }
    let current_repos: Vec<(RepositoryRef, String)> = repo_targets
        .iter()
        .map(|r| {
            (
                kopiur_api::common::normalized_repository_ref(r, namespace),
                kopiur_api::common::repo_key(r, namespace),
            )
        })
        .collect();
    let existing_entries = config
        .status
        .as_ref()
        .map(|s| s.verification.clone())
        .unwrap_or_default();
    let stamps = config
        .status
        .as_ref()
        .map(|s| s.verification_stamps.clone())
        .unwrap_or_default();
    let folded = crate::verification::fold_verification(
        &current_repos,
        &existing_entries,
        &stamps,
        namespace,
    );
    Some(MultiVerification {
        current_repos,
        folded,
    })
}

/// **Pure.** The kstatus conditions for the final policy status write: Ready
/// when every repository is, Reconciling (reason `RepositoryNotReady`) plus the
/// BLOCKED `RepositoriesReady` gate otherwise. On the all-ready side the gate
/// is cleared to `True` only when the condition already exists — a policy that
/// was never gated never grows the condition, keeping the healthy single-repo
/// status wire byte-identical.
fn policy_ready_conditions(
    existing: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition],
    generation: Option<i64>,
    all_ready: bool,
    gate_message: &str,
) -> Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> {
    if all_ready {
        let conditions = io::set_ready(
            existing,
            generation,
            io::ReadyOutcome::Ready,
            "Reconciled",
            "SnapshotPolicy reconciled; retention enforced",
        );
        if existing
            .iter()
            .any(|c| c.type_ == crate::consts::REPOSITORIES_READY_CONDITION)
        {
            io::upsert_condition(
                &conditions,
                crate::consts::REPOSITORIES_READY_CONDITION,
                true,
                "AllRepositoriesReady",
                "every referenced repository is Ready",
                generation,
            )
        } else {
            conditions
        }
    } else {
        let conditions = io::set_ready(
            existing,
            generation,
            io::ReadyOutcome::Reconciling,
            crate::consts::REPOSITORY_NOT_READY_REASON,
            gate_message,
        );
        io::upsert_gate(
            &conditions,
            &kopiur_api::gates::POLICY_REPOSITORY_NOT_READY_GATE,
            gate_message,
            generation,
        )
    }
}

/// **Pure-ish** (no IO): the deep-verify scratch no-op state to surface, over
/// every READY target — any repo whose MERGED scratch config ignores its
/// storageClass flags the policy; else the first evaluable state so the
/// condition self-clears.
fn select_scratch_state(
    config: &SnapshotPolicy,
    ready: &[&PolicyRepoTarget],
) -> Option<crate::verification::ScratchStorageClassState> {
    let v = config.spec.verification.as_ref()?;
    let mut first = None;
    for t in ready {
        match crate::verification::scratch_storage_class_state(&t.repo, v) {
            Some(st) if st.ignored => return Some(st),
            Some(st) if first.is_none() => first = Some(st),
            Some(_) | None => {}
        }
    }
    first
}

/// Build the final guarded status body: observedGeneration + conditions +
/// `lastSuccessfulSnapshot` (only when known — never thrashed with null) +
/// `repositorySummary` + the folded multi-repo verification state.
///
/// Multi-repo verification writes: the folded `verification` Vec, the flat
/// `lastVerified` (the fold's MIN — explicit `null` ONCE when it regresses to
/// unknown while a prior value exists, e.g. a just-added unverified repo),
/// and per-key `null`s pruning `verificationStamps` entries for repositories
/// no longer in the spec. A single-repo policy that previously was multi gets
/// its multi-only surfaces (`verification`, `verificationStamps`) nulled once.
/// Every conditional is keyed off the PRIOR status so the steady-state body
/// compares equal and the guarded write stays a no-op.
fn final_status_body(
    config: &SnapshotPolicy,
    generation: Option<i64>,
    conditions: Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition>,
    last_successful: Option<&str>,
    repo_targets: &[&RepositoryRef],
    folded: Option<&MultiVerification>,
) -> Result<serde_json::Value> {
    let names: Vec<&str> = repo_targets.iter().map(|r| r.name.as_str()).collect();
    let mut status = serde_json::json!({
        "observedGeneration": generation,
        "conditions": conditions,
        "repositorySummary": repository_summary_string(&names),
    });
    if let Some(ts) = last_successful {
        status["lastSuccessfulSnapshot"] = serde_json::json!(ts);
    }
    let prior = config.status.as_ref();
    match folded {
        Some(mv) => {
            status["verification"] = serde_json::to_value(&mv.folded.entries)?;
            let prior_flat = prior.is_some_and(|s| s.last_verified.is_some());
            match (&mv.folded.flat, prior_flat) {
                (Some(ts), _) => status["lastVerified"] = serde_json::json!(ts),
                // Regressed to unknown (a current repo has never verified)
                // while a prior value exists: clear it once, then stay silent.
                (None, true) => status["lastVerified"] = serde_json::Value::Null,
                (None, false) => {}
            }
            let current_keys: std::collections::BTreeSet<&str> =
                mv.current_repos.iter().map(|(_, k)| k.as_str()).collect();
            let stale: serde_json::Map<String, serde_json::Value> = prior
                .map(|s| {
                    s.verification_stamps
                        .keys()
                        .filter(|k| !current_keys.contains(k.as_str()))
                        .map(|k| (k.clone(), serde_json::Value::Null))
                        .collect()
                })
                .unwrap_or_default();
            if !stale.is_empty() {
                status["verificationStamps"] = serde_json::Value::Object(stale);
            }
        }
        None => {
            // multi→single edit: the multi-only surfaces would otherwise
            // linger forever. Null them exactly once (conditional on the prior
            // status actually carrying them).
            if prior.is_some_and(|s| !s.verification.is_empty()) {
                status["verification"] = serde_json::Value::Null;
            }
            if prior.is_some_and(|s| !s.verification_stamps.is_empty()) {
                status["verificationStamps"] = serde_json::Value::Null;
            }
        }
    }
    Ok(status)
}

/// The per-target verification scheduling loop (thin orchestration over
/// [`crate::verification::verify_step`]): the single-repo shape runs exactly
/// one step with the flat `lastVerified` anchor (byte-identical behavior); a
/// multi-repo policy runs one step per READY repository, each anchored on its
/// own folded entry and gated on ITS OWN #168 input. Returns the minimum
/// requested requeue.
async fn run_verify_steps(
    config: &SnapshotPolicy,
    ctx: &Context,
    namespace: &str,
    ready: &[&PolicyRepoTarget],
    folded: Option<&MultiVerification>,
    backups: &[Snapshot],
    has_successful_snapshot: bool,
) -> Result<Option<std::time::Duration>> {
    let parse_ts = |s: &str| {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    };
    let mut min_requeue: Option<std::time::Duration> = None;
    match folded {
        None => {
            if let Some(t) = ready.first() {
                let vt = crate::verification::VerifyTarget {
                    rref: &t.rref,
                    repo: &t.repo,
                    repo_key: None,
                    last_verified: config
                        .status
                        .as_ref()
                        .and_then(|s| s.last_verified.as_deref())
                        .and_then(parse_ts),
                    has_successful: has_successful_snapshot,
                };
                min_requeue = crate::verification::verify_step(config, ctx, &vt, namespace).await?;
            }
        }
        Some(mv) => {
            for t in ready {
                let key = kopiur_api::common::repo_key(&t.rref, namespace);
                let last = mv
                    .folded
                    .entries
                    .iter()
                    .find(|e| kopiur_api::common::repo_key(&e.repository, namespace) == key)
                    .and_then(|e| e.last_verified.as_deref())
                    .and_then(parse_ts);
                let vt = crate::verification::VerifyTarget {
                    rref: &t.rref,
                    repo: &t.repo,
                    has_successful: multi_repo_has_success(backups, &key),
                    repo_key: Some(key),
                    last_verified: last,
                };
                if let Some(rq) =
                    crate::verification::verify_step(config, ctx, &vt, namespace).await?
                {
                    min_requeue = Some(min_requeue.map_or(rq, |c| c.min(rq)));
                }
            }
        }
    }
    Ok(min_requeue)
}

/// One (repository, identity-under-that-repository) target of a live
/// `SnapshotPolicy`, resolved by `reconcile_inner` once per reconcile. The
/// unit of identity is the PAIR — a multi-repo policy legitimately resolves N
/// identities, one under each repository's `identityDefaults`.
pub(crate) struct PolicyRepoTarget {
    /// The spec's ref for this repository, as written.
    rref: RepositoryRef,
    /// The resolved repository surface.
    repo: io::ResolvedRepository,
    /// The policy's identity + sources resolved under THIS repository's
    /// `identityDefaults`.
    resolved: kopiur_api::snapshot_policy::ResolvedPolicy,
}

/// Server-side-apply ONLY `spec.repository` onto a child `Snapshot`, under a
/// dedicated field manager. A dedicated manager (not the shared apply manager)
/// is load-bearing: SSA treats an apply as the manager's FULL intent, so
/// applying this one field under the minting manager would shed its ownership
/// of every other spec field. Force is safe — no other manager claims
/// `spec.repository` on a produced child (mint stamps it only on new
/// multi-repo children, under the same value).
async fn backfill_spec_repository_pin(
    api: &Api<Snapshot>,
    cr_name: &str,
    body: serde_json::Value,
) -> Result<()> {
    use kube::api::{Patch, PatchParams};
    let mut patch = body;
    patch["apiVersion"] = serde_json::json!(kopiur_api::consts::API_VERSION);
    patch["kind"] = serde_json::json!("Snapshot");
    patch["metadata"] = serde_json::json!({ "name": cr_name });
    api.patch(
        cr_name,
        &PatchParams::apply("kopiur-repo-pin-backfill").force(),
        &Patch::Apply(&patch),
    )
    .await?;
    Ok(())
}

/// What one target's adoption pass contributed, aggregated by [`run_adoption`].
struct AdoptionOutcome {
    adopted: u64,
    requested_scan: bool,
    skipped_by_retention: u64,
    identity: String,
}

/// Shared, per-pass inputs threaded to each target's adoption pass — one
/// struct so [`adoption_pass_for_target`] stays under the argument lint and a
/// new shared input cannot be threaded to only SOME targets.
struct AdoptionShared<'a> {
    own_ids: &'a std::collections::BTreeSet<String>,
    has_history: bool,
    scan_requested_identity: Option<&'a str>,
    gate: &'a crate::adoption::AdoptionRetentionGate<'a>,
    now: &'a str,
    multi: bool,
}

/// One repository's slice of the adoption pass: gate → candidate LIST → pure
/// plan → execute → scan request + wave observability. `Ok(None)` when this
/// target's gates keep adoption inert (effective policy `Ignore`, or no
/// resolvable identity to match against).
async fn adoption_pass_for_target(
    ctx: &Context,
    config: &SnapshotPolicy,
    namespace: &str,
    name: &str,
    t: &PolicyRepoTarget,
    shared: AdoptionShared<'_>,
) -> Result<Option<AdoptionOutcome>> {
    use kopiur_api::common::{SnapshotAdoption, effective_adoption};

    // 1. Gate: only when the effective adoption policy (policy → repo → default)
    //    is Adopt. An unresolved identity (nothing to match against) is inert.
    if effective_adoption(config.spec.adoption, t.repo.catalog.as_ref()) != SnapshotAdoption::Adopt
    {
        return Ok(None);
    }
    let Some(policy_identity) = t.resolved.identity.as_ref() else {
        return Ok(None);
    };

    // 2. LIST discovered candidates cluster-wide (inv. 1), scoped to THIS repo.
    let repo_uid = t.repo.owner_ref.uid.as_str();
    let candidates = list_adoption_candidates(ctx, repo_uid).await?;
    let repo_cluster = t
        .repo
        .identity_defaults
        .as_ref()
        .and_then(|d| d.cluster.as_deref());

    // 4. Plan (pure).
    let plan = crate::adoption::plan_adoption(
        SnapshotAdoption::Adopt,
        policy_identity,
        repo_cluster,
        candidates,
        &crate::adoption::AdoptionHistory {
            own_snapshot_ids: shared.own_ids,
            has_history: shared.has_history,
            scan_requested_identity: shared.scan_requested_identity,
        },
        shared.gate,
    );

    // 5. Execute adoptions (inv. 4: create → ensure-status → delete discovered).
    let adopted = execute_adoptions(ctx, config, namespace, repo_uid, &t.rref, &plan.adopt).await?;
    let identity_str = kopiur_api::identity_string(policy_identity);

    // Multi-repo: `status.adoption.scanRequestedIdentity` is single-valued, so
    // the once-per-(policy, identity) no-match scan arm cannot keep its
    // guarantee across N per-repo identities (two repos would alternate the
    // stamp and re-request forever). Restrict that arm to the single-repo
    // shape — byte-identical there; adoption-WAVE scan requests (adopted > 0)
    // keep firing for every repo and converge on their own. Per-repo scan
    // bookkeeping rides M10's per-repo status wire.
    let request_scan = if shared.multi {
        !plan.adopt.is_empty()
    } else {
        plan.request_scan
    };

    // 6. On-demand catalog-scan request (stamp + Normal event).
    if request_scan {
        io::request_catalog_scan(&ctx.client, &t.rref, namespace, shared.now).await?;
        io::publish_normal_event(
            ctx,
            config,
            crate::consts::ADOPTION_SCAN_REQUESTED_REASON,
            crate::consts::AWAIT_CATALOG_SCAN_ACTION,
            &format!(
                "requested an on-demand catalog scan on repository {} so newly-recreated \
                 snapshots matching identity {identity_str} materialize for adoption",
                t.rref.name
            ),
        )
        .await;
    }

    // 7. Adoption-wave observability (metric + Normal event).
    if adopted > 0 {
        ctx.metrics.inc_snapshots_adopted(namespace, name, adopted);
        io::publish_normal_event(
            ctx,
            config,
            crate::consts::SNAPSHOTS_ADOPTED_REASON,
            crate::consts::REVIEW_ADOPTION_ACTION,
            &crate::adoption::adoption_event_message(adopted, &identity_str),
        )
        .await;
        tracing::info!(policy = %name, adopted, identity = %identity_str, "auto-adopted discovered snapshots");
    }

    Ok(Some(AdoptionOutcome {
        adopted,
        requested_scan: request_scan,
        skipped_by_retention: plan.skipped_by_retention,
        identity: identity_str,
    }))
}

/// Run one auto-adoption pass for a live `SnapshotPolicy` (M6, fixes #210),
/// AFTER the retention block (inv. 6). `reconcile_inner` only gains the call.
///
/// Loops the (repository, per-repo identity) pairs `reconcile_inner` resolved
/// (#368) — the READY subset only, since adopting against a down repository
/// has nothing to match and its discovered rows aren't going anywhere: each
/// repository's discovered rows are matched against the identity resolved
/// under THAT repository's `identityDefaults`. For the single-repo shape this
/// is exactly one iteration — behavior byte-identical to before.
///
/// `policy_is_multi` is the POLICY shape ([`kopiur_api::is_multi_repo`]), not
/// `targets.len() > 1`: a two-repo policy with one repo down still has a
/// single-valued `status.adoption.scanRequestedIdentity` that cannot keep the
/// once-per-identity no-match-scan guarantee, so the multi restriction must
/// not silently lift while the fleet is degraded.
///
/// Returns `Some(requeue)` when this pass adopted rows OR stamped an on-demand
/// catalog-scan request (a belt requeue so the wave settles promptly); `None`
/// means nothing happened — fall through to the normal steady-state return.
///
/// `backups` is the retention pass's `CONFIG_LABEL` child LIST, reused here to
/// derive this policy's own kopia ids + whether it has any history (never
/// re-LISTed). The discovered candidates come from a SEPARATE cluster-wide
/// LIST per repository.
#[allow(clippy::too_many_arguments)]
async fn run_adoption(
    ctx: &Context,
    api: &Api<SnapshotPolicy>,
    config: &SnapshotPolicy,
    namespace: &str,
    name: &str,
    targets: &[&PolicyRepoTarget],
    policy_is_multi: bool,
    backups: &[Snapshot],
    current: Option<&serde_json::Value>,
) -> Result<Option<std::time::Duration>> {
    // Own kopia ids + history from the retention child LIST (no re-LIST) —
    // shared across targets: kopia snapshot ids are unique per row regardless
    // of which repository holds them.
    let (own_ids, has_history) = own_snapshot_ids_and_history(backups);
    let scan_requested_identity = config
        .status
        .as_ref()
        .and_then(|s| s.adoption.as_ref())
        .and_then(|a| a.scan_requested_identity.as_deref());

    // Retention gate inputs (adoption inv. 8), from the SAME pre-prune `backups`
    // slice the retention pass just evaluated. That pre-prune evaluation matches
    // what the next retention pass will decide because `select_kept` is
    // time-invariant (buckets derive purely from end times), removing a non-kept
    // row never changes any other row's bucket outcome, and equal-end_time ties
    // break by id — with candidate views carrying the future adopted CR name, so
    // gate-time and next-pass tie-breaks are bit-identical. (Multi-repo note:
    // the gate simulates GFS flat over the policy's whole child set — a
    // conservative approximation of the per-(source, repo) buckets; the
    // per-repo gate refinement rides M10's per-repo status wire.)
    let own_views: Vec<SnapshotRetentionView> = backups.iter().filter_map(retention_view).collect();
    let gate = crate::adoption::AdoptionRetentionGate {
        retention: config.spec.retention.as_ref(),
        deletion_policy: crate::adoption::effective_deletion_policy(config),
        own_views: &own_views,
        policy_name: name,
    };

    // A single token for this pass: stamped on the repository AND recorded in
    // `status.adoption` so the two agree.
    let now = Utc::now().to_rfc3339();
    let multi = policy_is_multi;
    let mut outcomes: Vec<AdoptionOutcome> = Vec::new();
    for t in targets.iter().copied() {
        if let Some(outcome) = adoption_pass_for_target(
            ctx,
            config,
            namespace,
            name,
            t,
            AdoptionShared {
                own_ids: &own_ids,
                has_history,
                scan_requested_identity,
                gate: &gate,
                now: &now,
                multi,
            },
        )
        .await?
        {
            outcomes.push(outcome);
        }
    }
    // No target passed the adoption gates (Ignore, or no resolvable identity):
    // nothing ran, nothing to record — preserves the pre-#368 early return.
    if outcomes.is_empty() {
        return Ok(None);
    }
    let adopted = outcomes.iter().map(|o| o.adopted).sum::<u64>();
    let any_scan = outcomes.iter().any(|o| o.requested_scan);
    let identity_str = outcomes
        .iter()
        .rev()
        .find(|o| o.requested_scan)
        .or(outcomes.last())
        .map(|o| o.identity.clone())
        .unwrap_or_default();
    let plan_skipped = outcomes.iter().map(|o| o.skipped_by_retention).sum::<u64>();

    // 7b. Retention-gate observability (adoption inv. 8). Transition-gated: the
    //     event and the status write fire when the withheld COUNT changes, so a
    //     steady fully-withheld state publishes once and then stays byte-silent
    //     (status-churn rule).
    let prior_skipped = u64::from(
        config
            .status
            .as_ref()
            .and_then(|s| s.adoption.as_ref())
            .and_then(|a| a.skipped_by_retention)
            .unwrap_or(0),
    );
    let skipped_changed = plan_skipped != prior_skipped;
    if plan_skipped > 0 {
        tracing::debug!(
            policy = %name,
            skipped = plan_skipped,
            identity = %identity_str,
            "adoption withheld by the retention gate (inv. 8)"
        );
        if skipped_changed {
            io::publish_normal_event(
                ctx,
                config,
                crate::consts::ADOPTION_SKIPPED_BY_RETENTION_REASON,
                crate::consts::REVIEW_RETENTION_ACTION,
                &crate::adoption::adoption_skipped_event_message(plan_skipped, &identity_str),
            )
            .await;
        }
    }

    // 8. `status.adoption` summary (guarded, prior-carrying — only touched when
    //    there is activity, so a steady-state pass writes nothing).
    if adopted > 0 || any_scan || skipped_changed {
        write_adoption_status(
            api,
            name,
            current,
            config,
            adopted,
            any_scan,
            plan_skipped,
            &now,
            &identity_str,
        )
        .await?;
        return Ok(Some(std::time::Duration::from_secs(30)));
    }
    Ok(None)
}

/// LIST this repository's discovered rows cluster-wide (inv. 1) via the install
/// scope, selected by `origin: discovered` + the repository UID, and distill
/// them into [`crate::adoption::AdoptionCandidate`]s.
async fn list_adoption_candidates(
    ctx: &Context,
    repo_uid: &str,
) -> Result<Vec<crate::adoption::AdoptionCandidate>> {
    let api: Api<Snapshot> = crate::controllers::scoped_api(&ctx.client, &ctx.watch_scope);
    let lp = ListParams::default().labels(&format!(
        "{}={},{}={repo_uid}",
        crate::consts::ORIGIN_LABEL,
        Origin::Discovered.label_value(),
        crate::consts::REPOSITORY_UID_LABEL,
    ));
    let rows = api.list(&lp).await?.items;
    Ok(crate::adoption::adoption_candidates(repo_uid, &rows))
}

/// **Pure.** From this policy's `CONFIG_LABEL` children: the set of kopia ids
/// they already carry (so an adopted row is never re-adopted), and whether the
/// policy has any HISTORY — a retention-visible (`Succeeded`) row or an already-
/// `Adopted` row. "No history" is what lets a brand-new / recreated policy ask
/// for exactly one on-demand scan.
fn own_snapshot_ids_and_history(
    backups: &[Snapshot],
) -> (std::collections::BTreeSet<String>, bool) {
    let mut ids = std::collections::BTreeSet::new();
    let mut has_history = false;
    for b in backups {
        if let Some(info) = b
            .status
            .as_ref()
            .and_then(|s| s.snapshot.as_ref())
            .filter(|i| !i.kopia_snapshot_id.is_empty())
        {
            ids.insert(info.kopia_snapshot_id.clone());
        }
        // The `Adopted` arm requires the SAME controller-written provenance
        // (`status.snapshot`) as `retention_view` — a forged bare `origin: adopted`
        // label with no status must NOT count as history, or it would suppress a
        // recreated policy's one on-demand scan. (`retention_view` already requires
        // provenance, so a genuine adopted row that has converged to `Succeeded` is
        // caught by the first arm; this arm additionally counts an interim
        // controller-written adopted row before its phase pins.)
        let adopted_with_provenance = crate::snapshot::resolve_origin(b) == Some(Origin::Adopted)
            && b.status
                .as_ref()
                .and_then(|s| s.snapshot.as_ref())
                .is_some();
        if retention_view(b).is_some() || adopted_with_provenance {
            has_history = true;
        }
    }
    (ids, has_history)
}

/// Execute the planned adoptions in order, returning how many succeeded. Each is
/// idempotent and 404/409-tolerant ([`adopt_one`]), so a crash mid-pass re-runs
/// safely next reconcile.
async fn execute_adoptions(
    ctx: &Context,
    config: &SnapshotPolicy,
    namespace: &str,
    repo_uid: &str,
    repo_ref: &RepositoryRef,
    adopt: &[crate::adoption::AdoptionCandidate],
) -> Result<u64> {
    let mut count = 0u64;
    for candidate in adopt {
        adopt_one(ctx, config, namespace, repo_uid, repo_ref, candidate).await?;
        count += 1;
    }
    Ok(count)
}

/// **Pure.** Whether the object found at the adopted row's name on a create-409
/// is really THIS candidate's adopted row (a prior wave that crashed before the
/// status patch), and not a name-collision STRANGER. Requires the matching
/// `SNAPSHOT_ID_LABEL` AND a catalog origin label (`adopted`/`discovered`). On a
/// mismatch [`adopt_one`] skips the status patch (and the discovered-row delete)
/// rather than clobber a stranger's status.
fn adopted_row_matches_candidate(existing: &Snapshot, snapshot_id: &str) -> bool {
    let labels = existing.labels();
    let id_matches = labels
        .get(crate::consts::SNAPSHOT_ID_LABEL)
        .map(String::as_str)
        == Some(snapshot_id);
    let origin = labels.get(crate::consts::ORIGIN_LABEL).map(String::as_str);
    let origin_is_catalog = origin == Some(Origin::Adopted.label_value())
        || origin == Some(Origin::Discovered.label_value());
    id_matches && origin_is_catalog
}

/// Adopt one discovered row (inv. 4): CREATE the adopted row FIRST in the
/// POLICY's namespace, ENSURE its status, THEN delete the discovered row in ITS
/// OWN namespace. On create-409 (a prior wave already created it) DO NOT
/// early-return — fall through to re-ensure the status so a crash between create
/// and status-patch is healed, but FIRST verify the occupant is really this
/// candidate's adopted row ([`adopted_row_matches_candidate`]): a name-collision
/// stranger must never have its status overwritten. Every step is 404/409-tolerant.
async fn adopt_one(
    ctx: &Context,
    config: &SnapshotPolicy,
    namespace: &str,
    repo_uid: &str,
    repo_ref: &RepositoryRef,
    candidate: &crate::adoption::AdoptionCandidate,
) -> Result<()> {
    let (adopted, status) =
        crate::adoption::build_adopted_snapshot(config, repo_uid, candidate, repo_ref);
    let cr_name = adopted.name_any();
    let policy_api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), namespace);

    match policy_api.create(&PostParams::default(), &adopted).await {
        Ok(_) => {}
        // Already exists (a prior wave crashed after create) — verify it is really
        // OUR adopted row before re-ensuring status; do NOT early-return on the
        // happy path.
        Err(kube::Error::Api(ae)) if ae.code == 409 => match policy_api.get_opt(&cr_name).await? {
            Some(existing) if adopted_row_matches_candidate(&existing, &candidate.snapshot_id) => {}
            Some(_) => {
                tracing::warn!(
                    policy = %config.name_any(),
                    adopted = %cr_name,
                    discovered = %candidate.name,
                    "adopted-row name is occupied by an object that is not this candidate's \
                     adopted row (label mismatch); skipping to avoid overwriting a stranger's status"
                );
                return Ok(());
            }
            // 409 then vanished (a concurrent delete): nothing to adopt this pass,
            // re-planned on the next reconcile.
            None => return Ok(()),
        },
        Err(e) => return Err(Error::Kube(e)),
    }
    io::patch_status(&policy_api, &cr_name, serde_json::to_value(&status)?).await?;

    // Delete the discovered row in ITS OWN namespace, 404-tolerant.
    let disc_api: Api<Snapshot> = Api::namespaced(ctx.client.clone(), &candidate.namespace);
    match disc_api
        .delete(&candidate.name, &DeleteParams::default())
        .await
    {
        Ok(_) => {}
        Err(kube::Error::Api(ae)) if ae.code == 404 => {}
        Err(e) => return Err(Error::Kube(e)),
    }
    tracing::info!(
        policy = %config.name_any(),
        adopted = %cr_name,
        discovered = %candidate.name,
        "adopted a discovered snapshot"
    );
    Ok(())
}

/// Merge-patch `status.adoption` with the pass's summary, carrying prior values
/// forward for the fields this pass did not touch so [`io::patch_status_if_changed`]
/// stays a no-op in steady state.
#[allow(clippy::too_many_arguments)]
async fn write_adoption_status(
    api: &Api<SnapshotPolicy>,
    name: &str,
    current: Option<&serde_json::Value>,
    config: &SnapshotPolicy,
    adopted: u64,
    request_scan: bool,
    skipped_by_retention: u64,
    now: &str,
    identity: &str,
) -> Result<()> {
    use kopiur_api::snapshot_policy::AdoptionSummary;
    let prior = config.status.as_ref().and_then(|s| s.adoption.as_ref());
    let prior_total = prior.and_then(|a| a.total_adopted).unwrap_or(0);
    let summary = AdoptionSummary {
        // The CURRENT pass's gate outcome, not prior-carried: this is a live
        // gauge of how many matching rows are deliberately left discovered.
        skipped_by_retention: Some(u32::try_from(skipped_by_retention).unwrap_or(u32::MAX)),
        last_adoption_at: if adopted > 0 {
            Some(now.to_string())
        } else {
            prior.and_then(|a| a.last_adoption_at.clone())
        },
        last_adopted_count: if adopted > 0 {
            Some(adopted as u32)
        } else {
            prior.and_then(|a| a.last_adopted_count)
        },
        total_adopted: Some(prior_total + adopted),
        scan_requested_at: if request_scan {
            Some(now.to_string())
        } else {
            prior.and_then(|a| a.scan_requested_at.clone())
        },
        scan_requested_identity: if request_scan {
            Some(identity.to_string())
        } else {
            prior.and_then(|a| a.scan_requested_identity.clone())
        },
    };
    io::patch_status_if_changed(
        api,
        name,
        current,
        serde_json::json!({ "adoption": summary }),
    )
    .await?;
    Ok(())
}

/// Pure: the max `status.timing.endTime` over `Succeeded` Snapshots, as RFC3339
/// (backs `status.lastSuccessfulSnapshot`, §3). Fed the per-reconcile `backups`
/// slice — unit-tested without a cluster.
pub fn latest_successful_end_time(backups: &[Snapshot]) -> Option<String> {
    backups
        .iter()
        .filter_map(retention_view)
        .map(|v| v.end_time)
        .max()
        .map(|t| t.to_rfc3339())
}

/// Resolve a `SnapshotPolicy`'s identity into the api `ResolvedIdentity` (reused by
/// the restore reconciler for `fromPolicy` source resolution).
pub fn config_identity(
    config: &SnapshotPolicy,
    namespace: &str,
    defaults: Option<&kopiur_api::IdentityDefaults>,
) -> Result<kopiur_api::common::ResolvedIdentity> {
    let first = config.spec.sources.first();
    let pvc_name = first.and_then(|s| s.pvc.as_ref().map(|p| p.name.clone()));
    let nfs_source_path = first.and_then(|s| s.nfs.as_ref().map(|n| n.path.clone()));
    let source_path_override = first.and_then(|s| s.source_path_override.clone());
    let inputs = kopiur_api::IdentityInputs {
        object_name: &config.name_any(),
        namespace,
        overrides: config.spec.identity.as_ref(),
        defaults,
        labels: config.metadata.labels.as_ref(),
        annotations: config.metadata.annotations.as_ref(),
        pvc_name: pvc_name.as_deref(),
        default_source_path: nfs_source_path.as_deref(),
        source_path_override: source_path_override.as_deref(),
    };
    kopiur_api::resolve_identity(&inputs).map_err(|e| Error::Validation(e.to_string()))
}

/// **Pure.** The `status.resolved` mirror body for this pass (#368): the
/// single-repo shape pins its one resolution verbatim (byte-identical wire);
/// the multi shape pins one `repositories` entry per member — the repository
/// as listed plus the identity resolved under ITS `identityDefaults`, which is
/// the admission fork guard's per-repo baseline — and deliberately leaves the
/// top-level `identity` absent (no member is "the" identity).
fn resolved_policy_mirror(
    is_multi: bool,
    targets: &[PolicyRepoTarget],
) -> kopiur_api::snapshot_policy::ResolvedPolicy {
    use kopiur_api::snapshot_policy::{ResolvedPolicy, ResolvedPolicyRepository};
    if !is_multi {
        return targets
            .first()
            .map(|t| t.resolved.clone())
            .unwrap_or_default();
    }
    ResolvedPolicy {
        identity: None,
        // Sources resolve purely from the spec (PVC names / path overrides),
        // identically under every repository — mirrored once.
        sources: targets
            .first()
            .map(|t| t.resolved.sources.clone())
            .unwrap_or_default(),
        repositories: targets
            .iter()
            .map(|t| ResolvedPolicyRepository {
                repository: t.rref.clone(),
                identity: t.resolved.identity.clone(),
            })
            .collect(),
    }
}

/// Resolve the config's identity + per-source paths into a `ResolvedPolicy`
/// status body. Reuses `api::identity::resolve_identity` (tested kernel).
fn resolve_config_identity(
    config: &SnapshotPolicy,
    namespace: &str,
    defaults: Option<&kopiur_api::IdentityDefaults>,
) -> Result<kopiur_api::snapshot_policy::ResolvedPolicy> {
    use kopiur_api::snapshot_policy::{ResolvedPolicy, ResolvedPolicySource};
    let first = config.spec.sources.first();
    let pvc_name = first.and_then(|s| s.pvc.as_ref().map(|p| p.name.clone()));
    let nfs_source_path = first.and_then(|s| s.nfs.as_ref().map(|n| n.path.clone()));
    let source_path_override = first.and_then(|s| s.source_path_override.clone());
    let inputs = kopiur_api::IdentityInputs {
        object_name: &config.name_any(),
        namespace,
        overrides: config.spec.identity.as_ref(),
        defaults,
        labels: config.metadata.labels.as_ref(),
        annotations: config.metadata.annotations.as_ref(),
        pvc_name: pvc_name.as_deref(),
        default_source_path: nfs_source_path.as_deref(),
        source_path_override: source_path_override.as_deref(),
    };
    let identity =
        kopiur_api::resolve_identity(&inputs).map_err(|e| Error::Validation(e.to_string()))?;
    let sources = config
        .spec
        .sources
        .iter()
        .map(|s| ResolvedPolicySource {
            pvc: s.pvc.as_ref().map(|p| format!("{namespace}/{}", p.name)),
            source_path: s
                .source_path_override
                .clone()
                .or_else(|| s.pvc.as_ref().map(|p| format!("/pvc/{}", p.name)))
                .or_else(|| s.nfs.as_ref().map(|n| n.path.clone())),
        })
        .collect();
    Ok(ResolvedPolicy {
        identity: Some(identity),
        sources,
        // Per-target resolution: the multi-repo `repositories` mirror is
        // assembled across targets by `resolved_policy_mirror`, never here.
        repositories: Vec::new(),
    })
}

/// `error_policy` for the `SnapshotPolicy` controller.
pub fn error_policy(obj: Arc<SnapshotPolicy>, err: &Error, ctx: Arc<Context>) -> Action {
    error_policy_for("SnapshotPolicy", obj.as_ref(), err, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use kopiur_api::common::ResolvedIdentity;
    use kopiur_api::snapshot::{SnapshotInfo, SnapshotSpec, SnapshotStatus, SnapshotTiming};
    use kopiur_api::{Origin, SnapshotPhase};
    use std::collections::{BTreeMap, BTreeSet};

    fn at(y: i32, mo: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, 2, 0, 0).single().unwrap()
    }

    fn succeeded_backup(name: &str, end: DateTime<Utc>) -> Snapshot {
        let mut b = Snapshot::new(
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
        b.status = Some(SnapshotStatus {
            phase: Some(SnapshotPhase::Succeeded),
            origin: Some(Origin::Scheduled),
            timing: Some(SnapshotTiming {
                start_time: None,
                end_time: Some(end.to_rfc3339()),
                duration_seconds: None,
            }),
            snapshot: Some(SnapshotInfo {
                kopia_snapshot_id: format!("snap-{name}"),
                identity: ResolvedIdentity {
                    username: "u".into(),
                    hostname: "h".into(),
                    source_path: Some("/d".into()),
                },
                description: None,
            }),
            ..Default::default()
        });
        b
    }

    fn failed_backup(name: &str, end: DateTime<Utc>) -> Snapshot {
        let mut b = succeeded_backup(name, end);
        if let Some(s) = b.status.as_mut() {
            s.phase = Some(SnapshotPhase::Failed);
            s.snapshot = None;
        }
        b
    }

    fn unchanged_backup(name: &str, end: DateTime<Utc>) -> Snapshot {
        // What the mover actually writes for a deduped run: a terminal phase and
        // real timing, but NO `status.snapshot` — it owns no kopia manifest.
        let mut b = succeeded_backup(name, end);
        if let Some(s) = b.status.as_mut() {
            s.phase = Some(SnapshotPhase::Unchanged);
            s.snapshot = None;
        }
        b
    }

    fn policy(latest: Option<u32>, daily: Option<u32>) -> Retention {
        Retention {
            keep_latest: latest,
            keep_daily: daily,
            ..Default::default()
        }
    }

    fn fanned_backup(name: &str, pvc: &str, end: DateTime<Utc>) -> Snapshot {
        let mut b = succeeded_backup(name, end);
        b.spec.source = Some(kopiur_api::SnapshotSourceRef {
            source_index: 0,
            target: kopiur_api::SnapshotSourceTarget::Pvc(kopiur_api::PvcTargetRef {
                namespace: "ns".into(),
                name: pvc.into(),
            }),
            group: None,
        });
        b
    }

    // --- #346: GFS is per SOURCE, not per policy ---------------------------

    #[test]
    fn retention_keeps_n_days_per_pvc_not_n_days_across_all_of_them() {
        // The data-loss bug the fan-out would introduce. `select_kept` has no
        // grouping key, so one flat pass over a 3-PVC × 3-day population under
        // `keepDaily: 3` would keep 3 SNAPSHOTS TOTAL — one day — and delete the
        // other 6, silently destroying two days of every volume.
        let day = |d: u32| Utc.with_ymd_and_hms(2026, 8, d, 2, 0, 0).unwrap();
        let mut backups = Vec::new();
        for pvc in ["a", "b", "c"] {
            for d in 3..=5 {
                backups.push(fanned_backup(&format!("{pvc}-{d}"), pvc, day(d)));
            }
        }
        let del = backups_to_delete(&backups, &policy(None, Some(3)), false);
        assert!(
            del.is_empty(),
            "3 days × 3 PVCs under keepDaily=3 must keep everything, got deletes: {del:?}"
        );

        // And the bucketing really is per PVC: a 4th day evicts the oldest of
        // EACH volume, not three days of one.
        for pvc in ["a", "b", "c"] {
            backups.push(fanned_backup(&format!("{pvc}-6"), pvc, day(6)));
        }
        let del: std::collections::BTreeSet<String> =
            backups_to_delete(&backups, &policy(None, Some(3)), false)
                .into_iter()
                .collect();
        assert_eq!(
            del,
            ["a-3".to_string(), "b-3".to_string(), "c-3".to_string()]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn an_unfanned_policy_is_a_single_bucket_and_behaves_as_before() {
        // The compatibility half: every existing single-source policy has no
        // `spec.source`, shares the empty key, and must be untouched.
        let day = |d: u32| Utc.with_ymd_and_hms(2026, 8, d, 2, 0, 0).unwrap();
        let backups: Vec<Snapshot> = (1..=4)
            .map(|d| succeeded_backup(&format!("d{d}"), day(d)))
            .collect();
        for b in &backups {
            assert_eq!(retention_group_key(b, false), "");
        }
        let del = backups_to_delete(&backups, &policy(None, Some(2)), false);
        assert_eq!(del.len(), 2, "keepDaily=2 over 4 days deletes the oldest 2");
    }

    // --- #351: Unchanged is a success that owns nothing --------------------

    #[test]
    fn unchanged_breaks_the_consecutive_failure_streak() {
        // A run that read the source and found it identical is proof the backup
        // path works, so it must reset the streak exactly like a Succeeded run.
        // Without this the streak freezes at its last value and
        // `KopiurBackupFailing` can never clear for a policy that recovered and
        // then went quiet.
        let t = |h: u32| Utc.with_ymd_and_hms(2026, 8, 5, h, 0, 0).unwrap();
        let backups = [
            unchanged_backup("newest", t(6)),
            failed_backup("f2", t(5)),
            failed_backup("f1", t(4)),
        ];
        assert_eq!(consecutive_failures(backups.iter()), 0);

        // ...and failures AFTER the unchanged run still count.
        let backups = [
            failed_backup("f3", t(7)),
            unchanged_backup("u", t(6)),
            failed_backup("f2", t(5)),
        ];
        assert_eq!(consecutive_failures(backups.iter()), 1);
    }

    #[test]
    fn unchanged_never_participates_in_gfs_retention() {
        // It holds no restore point, so it must not occupy a GFS slot and evict
        // one that does. `retention_view` already gates on Succeeded + a
        // recorded status.snapshot; this pins that behavior against the new phase.
        let t = |d: u32| Utc.with_ymd_and_hms(2026, 8, d, 2, 0, 0).unwrap();
        let real = succeeded_backup("real", t(1));
        let dedup = unchanged_backup("dedup", t(2));
        assert!(retention_view(&dedup).is_none());

        // keepLatest: 1 with one real + one unchanged must keep the REAL one.
        let del = backups_to_delete(&[real, dedup], &policy(Some(1), None), false);
        assert!(
            del.is_empty(),
            "the only real snapshot must survive keepLatest=1, got {del:?}"
        );
    }

    #[test]
    fn unchanged_rows_are_bounded_by_the_history_limit() {
        // Neither GFS (no manifest) nor failedJobsHistoryLimit (only counts
        // Failed) bounds these, so an hourly schedule over static data would
        // accrue them forever.
        let t = |h: u32| Utc.with_ymd_and_hms(2026, 8, 5, h, 0, 0).unwrap();
        let rows: Vec<Snapshot> = (0..5)
            .map(|i| unchanged_backup(&format!("u{i}"), t(i)))
            .collect();
        let pruned = unchanged_snapshots_to_prune(&rows, 2, false);
        // Newest two kept (u4, u3); the three oldest pruned.
        assert_eq!(pruned.len(), 3);
        assert!(!pruned.contains(&"u4".to_string()));
        assert!(!pruned.contains(&"u3".to_string()));
        assert!(pruned.contains(&"u0".to_string()));
    }

    #[test]
    fn the_history_bound_ignores_other_phases_and_terminating_rows() {
        let t = |h: u32| Utc.with_ymd_and_hms(2026, 8, 5, h, 0, 0).unwrap();
        let mut terminating = unchanged_backup("draining", t(0));
        terminating.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::now(),
            ));
        let rows = vec![
            succeeded_backup("real", t(3)),
            failed_backup("bad", t(2)),
            terminating,
        ];
        // Limit 0 would prune every Unchanged row — but none of these qualify.
        assert!(unchanged_snapshots_to_prune(&rows, 0, false).is_empty());
    }

    #[test]
    fn consecutive_failures_counts_trailing_failures_before_last_success() {
        // Newest→oldest: Fail(26), Fail(25), Succeed(24), Fail(23) → 2.
        let backups = vec![
            failed_backup("f23", at(2026, 5, 23)),
            succeeded_backup("s24", at(2026, 5, 24)),
            failed_backup("f25", at(2026, 5, 25)),
            failed_backup("f26", at(2026, 5, 26)),
        ];
        assert_eq!(consecutive_failures(&backups), 2);
        // All succeeded → 0.
        assert_eq!(
            consecutive_failures(&[succeeded_backup("s", at(2026, 5, 24))]),
            0
        );
        // All failed → counts them all.
        assert_eq!(
            consecutive_failures(&[
                failed_backup("f1", at(2026, 5, 24)),
                failed_backup("f2", at(2026, 5, 25)),
            ]),
            2
        );
        // No terminal backups (e.g. only Running/Pending) → 0.
        assert_eq!(consecutive_failures(&[] as &[Snapshot]), 0);
    }

    #[test]
    fn rfc3339_unix_secs_parses_last_verified_and_rejects_garbage() {
        // 2023-11-14T22:13:20Z == 1_700_000_000 (the value status.lastVerified
        // carries → kopiur_snapshot_verified_timestamp_seconds).
        assert_eq!(
            rfc3339_unix_secs("2023-11-14T22:13:20Z"),
            Some(1_700_000_000)
        );
        // Offset timestamps normalize to the same instant.
        assert_eq!(
            rfc3339_unix_secs("2023-11-14T23:13:20+01:00"),
            Some(1_700_000_000)
        );
        // Malformed → None (gauge stays unset rather than crashing the reconcile).
        assert_eq!(rfc3339_unix_secs("not-a-timestamp"), None);
        assert_eq!(rfc3339_unix_secs(""), None);
    }

    #[test]
    fn keeps_newest_deletes_rest_via_retention_kernel() {
        let backups = vec![
            succeeded_backup("d24", at(2026, 5, 24)),
            succeeded_backup("d23", at(2026, 5, 23)),
            succeeded_backup("d22", at(2026, 5, 22)),
        ];
        let del: BTreeSet<String> = backups_to_delete(&backups, &policy(Some(1), None), false)
            .into_iter()
            .collect();
        assert_eq!(
            del,
            ["d23".to_string(), "d22".to_string()].into_iter().collect()
        );
    }

    // -- I1: provenance gates on adopted / retention rows ---------------------

    #[test]
    fn retention_view_requires_controller_written_provenance() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        // A Succeeded row with NO status.snapshot must NOT participate in GFS. This
        // is the exact shape a HEAD `pin_adopted_row` would mint for a forged
        // `origin: adopted` label: phase pinned Succeeded, no provenance. Its
        // creationTimestamp fallback would otherwise claim a GFS bucket and displace
        // a real snapshot into the (breaker-exempt) retention delete set.
        let mut phantom = succeeded_backup("phantom", at(2026, 5, 24));
        let s = phantom.status.as_mut().unwrap();
        s.snapshot = None; // strip controller-written provenance
        s.timing = None; // force the creationTimestamp fallback path
        phantom.metadata.creation_timestamp = Some(Time(
            k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
        ));
        assert!(
            retention_view(&phantom).is_none(),
            "a Succeeded row without status.snapshot never enters GFS (HEAD returned \
             Some via the creationTimestamp fallback)"
        );
        // A genuine produced/adopted row (status.snapshot present) still enters GFS.
        assert!(retention_view(&succeeded_backup("real", at(2026, 5, 24))).is_some());
        // A phase written by a NEWER operator never enters GFS either: the losers
        // of a retention pass get DELETED from the repository, so a phase this
        // build cannot read must not be able to claim (or lose) a bucket.
        let mut future = succeeded_backup("future", at(2026, 5, 24));
        future.status.as_mut().unwrap().phase = Some(SnapshotPhase::Unknown("Quiescing".into()));
        assert!(retention_view(&future).is_none());
    }

    #[test]
    fn has_history_requires_provenance_for_a_bare_adopted_label() {
        // A forged BARE `origin: adopted` label row (no status at all) must NOT
        // count as history — counting it would suppress a delete-then-recreated
        // policy's one on-demand catalog scan. On HEAD the `origin == Adopted` arm
        // set has_history from the label alone.
        let mut forged = succeeded_backup("forged", at(2026, 5, 24));
        forged.status = None; // bare row: label only, no controller-written status
        let mut labels = BTreeMap::new();
        labels.insert(
            crate::consts::ORIGIN_LABEL.to_string(),
            Origin::Adopted.label_value().to_string(),
        );
        forged.metadata.labels = Some(labels);
        assert_eq!(
            crate::snapshot::resolve_origin(&forged),
            Some(Origin::Adopted)
        );
        let (_ids, has_history) = own_snapshot_ids_and_history(&[forged]);
        assert!(
            !has_history,
            "a bare origin:adopted label with no status.snapshot is not history"
        );

        // A genuine adopted row (status.snapshot present) DOES count as history.
        let (_ids, has_history) =
            own_snapshot_ids_and_history(&[backup_with_origin("genuine", Origin::Adopted)]);
        assert!(has_history, "a controller-written adopted row is history");
    }

    #[test]
    fn adopted_row_matches_candidate_guards_name_collision_strangers() {
        let row = |id: Option<&str>, origin: Option<&str>| {
            let mut b = Snapshot::new(
                "occupant",
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
            let mut labels = BTreeMap::new();
            if let Some(id) = id {
                labels.insert(crate::consts::SNAPSHOT_ID_LABEL.to_string(), id.to_string());
            }
            if let Some(o) = origin {
                labels.insert(crate::consts::ORIGIN_LABEL.to_string(), o.to_string());
            }
            b.metadata.labels = Some(labels);
            b
        };
        // Our adopted row (a prior wave's crash-before-status-patch) → matches.
        assert!(adopted_row_matches_candidate(
            &row(Some("abc"), Some("adopted")),
            "abc"
        ));
        // A discovered-origin occupant at the name is also a catalog row → matched.
        assert!(adopted_row_matches_candidate(
            &row(Some("abc"), Some("discovered")),
            "abc"
        ));
        // Wrong snapshot id → stranger (name collision on the first-16 id prefix).
        assert!(!adopted_row_matches_candidate(
            &row(Some("xyz"), Some("adopted")),
            "abc"
        ));
        // Non-catalog origin (a produced snapshot) → stranger.
        assert!(!adopted_row_matches_candidate(
            &row(Some("abc"), Some("scheduled")),
            "abc"
        ));
        // Missing labels entirely → stranger.
        assert!(!adopted_row_matches_candidate(&row(None, None), "abc"));
    }

    /// Mark a Snapshot terminating, optionally with a valid `pruned-by` stamp.
    fn terminating(mut b: Snapshot, pruned: bool) -> Snapshot {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        b.metadata.deletion_timestamp = Some(Time(
            k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
        ));
        if pruned {
            let mut ann = BTreeMap::new();
            ann.insert(
                kopiur_api::consts::PRUNED_BY_ANNOTATION.to_string(),
                PrunedBy::Retention.annotation_value().to_string(),
            );
            b.metadata.annotations = Some(ann);
        }
        b
    }

    #[test]
    fn partition_retention_prune_splits_live_delete_from_terminating_stamp() {
        let backups = vec![
            succeeded_backup("live", at(2026, 5, 22)),
            terminating(succeeded_backup("term-unstamped", at(2026, 5, 21)), false),
            terminating(succeeded_backup("term-stamped", at(2026, 5, 20)), true),
        ];
        let selected = vec![
            "live".to_string(),
            "term-unstamped".to_string(),
            "term-stamped".to_string(),
            "already-gone".to_string(), // selected but not in the live set → skip
        ];
        let (to_delete, to_stamp_only) = partition_retention_prune(&backups, &selected);
        // Live selected → normal stamp-then-delete.
        assert_eq!(to_delete, vec!["live".to_string()]);
        // Terminating + no pruned-by → stamp only (reclassify draining prune).
        assert_eq!(to_stamp_only, vec!["term-unstamped".to_string()]);
        // Terminating + already stamped, and the absent one, are in neither set.
    }

    // -- plan_policy_cascade (pure decision layer; executed by the M3 finalizer) --

    /// A `Succeeded` Snapshot with the given `origin` (status-first, matching
    /// `resolve_origin`'s precedence).
    fn backup_with_origin(name: &str, origin: Origin) -> Snapshot {
        let mut b = succeeded_backup(name, at(2026, 5, 24));
        if let Some(s) = b.status.as_mut() {
            s.origin = Some(origin);
        }
        b
    }

    #[test]
    fn plan_policy_cascade_retain_mode_mixed_population() {
        let children = vec![
            backup_with_origin("live-produced", Origin::Scheduled),
            backup_with_origin("live-adopted", Origin::Adopted),
            backup_with_origin("live-discovered", Origin::Discovered),
            terminating(backup_with_origin("term-unstamped", Origin::Manual), false),
            terminating(backup_with_origin("term-stamped", Origin::Scheduled), true),
        ];
        let plan = plan_policy_cascade(&children, PolicyDeletePolicy::Retain);
        let stamp_and_delete: BTreeSet<String> = plan.stamp_and_delete.into_iter().collect();
        assert_eq!(
            stamp_and_delete,
            ["live-produced".to_string(), "live-adopted".to_string()]
                .into_iter()
                .collect(),
            "discovered is excluded; adopted is cascaded like produced"
        );
        assert_eq!(plan.stamp_only, vec!["term-unstamped".to_string()]);
        assert!(
            plan.delete_only.is_empty(),
            "Retain mode never bare-deletes"
        );
    }

    #[test]
    fn plan_policy_cascade_delete_mode_mixed_population() {
        let children = vec![
            backup_with_origin("live-produced", Origin::Scheduled),
            backup_with_origin("live-adopted", Origin::Adopted),
            backup_with_origin("live-discovered", Origin::Discovered),
            terminating(backup_with_origin("term-unstamped", Origin::Manual), false),
            terminating(backup_with_origin("term-stamped", Origin::Scheduled), true),
        ];
        let plan = plan_policy_cascade(&children, PolicyDeletePolicy::Delete);
        let delete_only: BTreeSet<String> = plan.delete_only.into_iter().collect();
        assert_eq!(
            delete_only,
            ["live-produced".to_string(), "live-adopted".to_string()]
                .into_iter()
                .collect(),
            "discovered is excluded; adopted is cascaded like produced, bare-unstamped"
        );
        assert!(plan.stamp_and_delete.is_empty(), "Delete mode never stamps");
        assert!(
            plan.stamp_only.is_empty(),
            "Delete mode never touches terminating children"
        );
    }

    #[test]
    fn plan_policy_cascade_excludes_discovered_from_every_set_live_and_terminating() {
        // Defensive: a hand-labeled discovered CR must not churn just because a
        // policy it merely resembles was deleted — neither live nor terminating.
        let children = vec![
            backup_with_origin("live-discovered", Origin::Discovered),
            terminating(
                backup_with_origin("term-discovered", Origin::Discovered),
                false,
            ),
        ];
        for mode in [PolicyDeletePolicy::Retain, PolicyDeletePolicy::Delete] {
            let plan = plan_policy_cascade(&children, mode);
            assert!(plan.stamp_and_delete.is_empty(), "{mode:?}");
            assert!(plan.stamp_only.is_empty(), "{mode:?}");
            assert!(plan.delete_only.is_empty(), "{mode:?}");
        }
    }

    #[test]
    fn plan_policy_cascade_empty_input_is_empty_plan() {
        for mode in [PolicyDeletePolicy::Retain, PolicyDeletePolicy::Delete] {
            let plan = plan_policy_cascade(&[], mode);
            assert_eq!(plan, PolicyCascadePlan::default(), "{mode:?}");
        }
    }

    #[test]
    fn plan_policy_cascade_only_terminating_children_yields_no_live_work_in_either_mode() {
        // The release-condition guarantee: a population of ONLY breaker-held
        // terminating children must yield empty stamp_and_delete AND empty
        // delete_only in BOTH modes, or the M3 finalizer would wait forever on
        // a mass-deletion ack that can never come once the policy is gone.
        let children = vec![
            terminating(
                backup_with_origin("held-unstamped", Origin::Scheduled),
                false,
            ),
            terminating(backup_with_origin("held-stamped", Origin::Adopted), true),
        ];
        for mode in [PolicyDeletePolicy::Retain, PolicyDeletePolicy::Delete] {
            let plan = plan_policy_cascade(&children, mode);
            assert!(plan.stamp_and_delete.is_empty(), "{mode:?}");
            assert!(plan.delete_only.is_empty(), "{mode:?}");
        }
        // Retain mode still performs its documented non-blocking side effect
        // (stamping the unstamped one so its finalizer drains quietly); Delete
        // mode touches nothing in this population at all.
        let retain_plan = plan_policy_cascade(&children, PolicyDeletePolicy::Retain);
        assert_eq!(retain_plan.stamp_only, vec!["held-unstamped".to_string()]);
        let delete_plan = plan_policy_cascade(&children, PolicyDeletePolicy::Delete);
        assert!(delete_plan.stamp_only.is_empty());
    }

    // -- slice_policy_cascade_batch (M3 — the per-pass execution slice) --

    fn names(prefix: &str, ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| format!("{prefix}{id}")).collect()
    }

    #[test]
    fn slice_policy_cascade_batch_takes_everything_when_cap_covers_the_whole_plan() {
        let plan = PolicyCascadePlan {
            stamp_and_delete: names("sd", &["1", "2"]),
            stamp_only: names("so", &["1", "2"]),
            delete_only: names("do", &["1", "2"]),
        };
        let batch = slice_policy_cascade_batch(&plan, 50);
        assert_eq!(batch.stamp_and_delete, plan.stamp_and_delete.as_slice());
        assert_eq!(batch.stamp_only, plan.stamp_only.as_slice());
        assert_eq!(batch.delete_only, plan.delete_only.as_slice());
    }

    #[test]
    fn slice_policy_cascade_batch_prefers_stamp_and_delete_then_stamp_only_then_delete_only() {
        let plan = PolicyCascadePlan {
            stamp_and_delete: names("sd", &["1", "2"]),
            stamp_only: names("so", &["1", "2"]),
            delete_only: names("do", &["1", "2"]),
        };
        // Cap smaller than stamp_and_delete alone → only a truncated prefix of
        // stamp_and_delete; stamp_only/delete_only get nothing this pass.
        let tight = slice_policy_cascade_batch(&plan, 1);
        assert_eq!(tight.stamp_and_delete, &["sd1".to_string()]);
        assert!(tight.stamp_only.is_empty());
        assert!(tight.delete_only.is_empty());

        // Cap exactly covers stamp_and_delete plus part of stamp_only.
        let mid = slice_policy_cascade_batch(&plan, 3);
        assert_eq!(mid.stamp_and_delete, plan.stamp_and_delete.as_slice());
        assert_eq!(mid.stamp_only, &["so1".to_string()]);
        assert!(mid.delete_only.is_empty());

        // Cap covers everything but the last delete_only entry.
        let almost_all = slice_policy_cascade_batch(&plan, 5);
        assert_eq!(
            almost_all.stamp_and_delete,
            plan.stamp_and_delete.as_slice()
        );
        assert_eq!(almost_all.stamp_only, plan.stamp_only.as_slice());
        assert_eq!(almost_all.delete_only, &["do1".to_string()]);
    }

    #[test]
    fn slice_policy_cascade_batch_zero_cap_takes_nothing() {
        let plan = PolicyCascadePlan {
            stamp_and_delete: names("sd", &["1"]),
            stamp_only: names("so", &["1"]),
            delete_only: names("do", &["1"]),
        };
        let batch = slice_policy_cascade_batch(&plan, 0);
        assert!(batch.stamp_and_delete.is_empty());
        assert!(batch.stamp_only.is_empty());
        assert!(batch.delete_only.is_empty());
    }

    #[test]
    fn daily_policy_keeps_one_per_day() {
        let backups = vec![
            succeeded_backup("d24", at(2026, 5, 24)),
            succeeded_backup("d23", at(2026, 5, 23)),
            succeeded_backup("d22", at(2026, 5, 22)),
        ];
        // keepDaily:2 → newest two days kept, oldest deleted.
        let del = backups_to_delete(&backups, &policy(None, Some(2)), false);
        assert_eq!(del, vec!["d22".to_string()]);
    }

    #[test]
    fn non_terminal_backups_are_ignored() {
        // A Running backup has no end time and is not Succeeded → not a
        // retention candidate, so it is never returned for deletion.
        let mut running = Snapshot::new(
            "running",
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
        running.status = Some(SnapshotStatus {
            phase: Some(SnapshotPhase::Running),
            ..Default::default()
        });
        let succeeded = succeeded_backup("done", at(2026, 5, 24));
        let del = backups_to_delete(&[running, succeeded], &policy(Some(1), None), false);
        assert!(del.is_empty(), "single succeeded kept, running ignored");
    }

    #[test]
    fn empty_policy_marks_all_succeeded_for_deletion() {
        let backups = vec![
            succeeded_backup("a", at(2026, 5, 24)),
            succeeded_backup("b", at(2026, 5, 23)),
        ];
        let del: BTreeSet<String> = backups_to_delete(&backups, &Retention::default(), false)
            .into_iter()
            .collect();
        assert_eq!(
            del,
            ["a".to_string(), "b".to_string()].into_iter().collect()
        );
    }

    #[test]
    fn pinned_snapshot_is_never_pruned_by_gfs() {
        // §13(c): a pinned Snapshot is exempt — keepLatest:1 would delete the older
        // ones, but the pinned one survives.
        let mut pinned = succeeded_backup("pinned", at(2026, 5, 20));
        pinned.spec.pin = true;
        let backups = vec![
            succeeded_backup("newest", at(2026, 5, 24)),
            pinned,
            succeeded_backup("old", at(2026, 5, 19)),
        ];
        let del: BTreeSet<String> = backups_to_delete(&backups, &policy(Some(1), None), false)
            .into_iter()
            .collect();
        assert!(del.contains("old"), "unpinned old snapshot is pruned");
        assert!(!del.contains("pinned"), "pinned snapshot is never pruned");
        assert!(!del.contains("newest"));
    }

    #[test]
    fn latest_successful_end_time_is_the_max_succeeded() {
        // §3: the lastSuccessfulSnapshot is the newest Succeeded endTime; failures
        // and an empty set don't count.
        let backups = vec![
            succeeded_backup("a", at(2026, 5, 22)),
            succeeded_backup("b", at(2026, 5, 24)),
            failed_backup("f", at(2026, 5, 25)),
        ];
        assert_eq!(
            latest_successful_end_time(&backups),
            Some(at(2026, 5, 24).to_rfc3339())
        );
        assert_eq!(latest_successful_end_time(&[]), None);
        assert_eq!(
            latest_successful_end_time(&[failed_backup("f", at(2026, 5, 25))]),
            None
        );
    }

    // --- multi-repo retention bucketing + the spec-pin backfill (#368) -------

    /// Pin a mint-time `spec.repository` onto a fixture row (normalized form,
    /// as mint stamps it).
    fn pin(mut b: Snapshot, repo: &str) -> Snapshot {
        b.metadata.namespace = Some("apps".into());
        b.spec.repository = Some(kopiur_api::common::RepositoryRef {
            kind: kopiur_api::common::RepositoryKind::Repository,
            name: repo.into(),
            namespace: Some("apps".into()),
        });
        b
    }

    /// keepLatest-style bucketing keeps N EACH per (source, repo) for a pinned
    /// multi-repo population — the N repositories are independent captures.
    #[test]
    fn multi_repo_retention_keeps_n_per_source_repo_bucket() {
        let backups = vec![
            pin(succeeded_backup("a-old", at(2026, 5, 1)), "repo-a"),
            pin(succeeded_backup("a-new", at(2026, 5, 2)), "repo-a"),
            pin(succeeded_backup("b-old", at(2026, 5, 1)), "repo-b"),
            pin(succeeded_backup("b-new", at(2026, 5, 2)), "repo-b"),
        ];
        // keepLatest: 1, multi-repo buckets: each repo keeps ITS newest.
        let mut del = backups_to_delete(&backups, &policy(Some(1), None), true);
        del.sort();
        assert_eq!(del, vec!["a-old", "b-old"]);
        // The SAME population under a FLAT (single-repo) evaluation keeps one
        // TOTAL (a-new survives the equal-endTime tie-break) — which is
        // exactly why the repo dimension is load-bearing.
        let mut flat = backups_to_delete(&backups, &policy(Some(1), None), false);
        flat.sort();
        assert_eq!(flat, vec!["a-old", "b-new", "b-old"]);
    }

    /// Single→multi edit: pre-feature unpinned rows share the ""-repo bucket
    /// (transition state) until the backfill pass promotes their run-time pin;
    /// the pure decision selects exactly the promotable rows.
    #[test]
    fn repository_pin_backfill_selects_only_promotable_rows() {
        use kopiur_api::PolicyRef;
        // Promotable: policyRef + status pin + no spec pin.
        let mut promotable = succeeded_backup("p", at(2026, 5, 1));
        promotable.spec.policy_ref = Some(PolicyRef {
            name: "pg".into(),
            namespace: None,
        });
        promotable.status.as_mut().unwrap().resolved =
            Some(kopiur_api::snapshot::ResolvedSnapshot {
                repository: Some(kopiur_api::common::RepositoryRef {
                    kind: kopiur_api::common::RepositoryKind::Repository,
                    name: "repo-a".into(),
                    namespace: Some("apps".into()),
                }),
                ..Default::default()
            });
        // Already pinned: never re-selected (idempotence).
        let mut pinned_row = promotable.clone();
        pinned_row.metadata.name = Some("already".into());
        pinned_row.spec.repository = Some(kopiur_api::common::RepositoryRef {
            kind: kopiur_api::common::RepositoryKind::Repository,
            name: "repo-a".into(),
            namespace: Some("apps".into()),
        });
        // Neither pin: repository unknowable — stays unpinned, ages out.
        let neither = succeeded_backup("neither", at(2026, 5, 1));
        // Terminating: skipped (its finalizer already runs on captured state).
        let mut terminating = promotable.clone();
        terminating.metadata.name = Some("terminating".into());
        terminating.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
            ));
        // No policyRef (discovered/copy rows): not a policy child, never touched.
        let mut foreign = promotable.clone();
        foreign.metadata.name = Some("foreign".into());
        foreign.spec.policy_ref = None;

        let patches = repository_pin_backfill_patches(&[
            promotable,
            pinned_row,
            neither,
            terminating,
            foreign,
        ]);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].0, "p");
        assert_eq!(
            patches[0].1,
            serde_json::json!({ "spec": { "repository": {
                "kind": "Repository", "name": "repo-a", "namespace": "apps"
            } } })
        );
    }

    /// Multi→single edit: the flag comes from the CURRENT spec, so leftover
    /// pinned children merge back into the flat source-only buckets — the
    /// documented TRANSIENT GFS mixing that self-resolves as the removed
    /// repo's rows age out of every keep window.
    #[test]
    fn multi_to_single_edit_merges_pinned_rows_into_flat_buckets() {
        let backups = vec![
            // Leftovers pinned to the removed repo, plus the surviving repo's
            // rows — after the edit they all compete in ONE bucket.
            pin(succeeded_backup("removed-1", at(2026, 5, 3)), "removed"),
            pin(succeeded_backup("kept-1", at(2026, 5, 2)), "kept"),
            pin(succeeded_backup("kept-2", at(2026, 5, 1)), "kept"),
        ];
        let mut del = backups_to_delete(&backups, &policy(Some(1), None), false);
        del.sort();
        // Flat keepLatest:1 keeps only the newest overall ("removed-1", the
        // stale repo's row!) — the transient mixing this test documents. The
        // same rows under the multi flag would keep one per repo.
        assert_eq!(del, vec!["kept-1", "kept-2"]);
        assert_eq!(
            backups_to_delete(&backups, &policy(Some(1), None), true).len(),
            1,
            "multi buckets keep one per repo (only kept-2 falls out)"
        );
        // Pins are byte-ignored by the flat key: the delete set equals that of
        // the identical UNPINNED population (single-repo goldens hold even
        // when stray pins exist).
        let unpinned = vec![
            succeeded_backup("removed-1", at(2026, 5, 3)),
            succeeded_backup("kept-1", at(2026, 5, 2)),
            succeeded_backup("kept-2", at(2026, 5, 1)),
        ];
        let mut del_unpinned = backups_to_delete(&unpinned, &policy(Some(1), None), false);
        del_unpinned.sort();
        assert_eq!(del, del_unpinned);
    }

    /// `unchanged_snapshots_to_prune` inherits the (source, repo) key: each
    /// repo's Unchanged history is bounded independently under the multi flag.
    #[test]
    fn unchanged_prune_buckets_per_repo_under_the_multi_flag() {
        fn unchanged(name: &str, end: DateTime<Utc>) -> Snapshot {
            let mut b = succeeded_backup(name, end);
            if let Some(s) = b.status.as_mut() {
                s.phase = Some(SnapshotPhase::Unchanged);
                s.snapshot = None;
            }
            b
        }
        let rows = vec![
            pin(unchanged("a1", at(2026, 5, 1)), "repo-a"),
            pin(unchanged("a2", at(2026, 5, 2)), "repo-a"),
            pin(unchanged("b1", at(2026, 5, 1)), "repo-b"),
            pin(unchanged("b2", at(2026, 5, 2)), "repo-b"),
        ];
        let mut multi = unchanged_snapshots_to_prune(&rows, 1, true);
        multi.sort();
        assert_eq!(multi, vec!["a1", "b1"], "limit applies per repo");
        let mut flat = unchanged_snapshots_to_prune(&rows, 1, false);
        flat.sort();
        assert_eq!(flat, vec!["a1", "b1", "b2"], "flat keeps one total");
    }

    // --- M10: ready-subset execution filter -------------------------------

    #[test]
    fn executable_prunes_all_ready_passes_everything_through() {
        // The single-repo / fully-healthy path: byte-identical, never filtered
        // (including unpinned legacy rows and even names with no live row).
        let rows = vec![succeeded_backup("keep", at(2026, 5, 1))];
        let selected = vec!["keep".to_string(), "already-gone".to_string()];
        let ready = std::collections::BTreeSet::new();
        assert_eq!(
            executable_prunes(selected.clone(), &rows, &ready, true),
            selected
        );
    }

    #[test]
    fn executable_prunes_defers_down_repo_and_unpinned_rows() {
        // With repo-b down, only rows pinned to a READY repo execute: a delete
        // fires the finalizer, which contacts the row's repository. Unpinned
        // rows (repository unknowable from the spec) are deferred too.
        let rows = vec![
            pin(succeeded_backup("a1", at(2026, 5, 1)), "repo-a"),
            pin(succeeded_backup("b1", at(2026, 5, 1)), "repo-b"),
            succeeded_backup("legacy", at(2026, 5, 1)),
        ];
        let ready: std::collections::BTreeSet<String> =
            ["Repository/apps/repo-a".to_string()].into();
        let selected = vec!["a1".to_string(), "b1".to_string(), "legacy".to_string()];
        assert_eq!(
            executable_prunes(selected, &rows, &ready, false),
            vec!["a1".to_string()],
            "only the ready repo's pinned rows may prune while part of the fleet is down"
        );
    }

    // --- M10: repositorySummary print column ------------------------------

    #[test]
    fn repository_summary_renders_single_multi_and_overflow() {
        assert_eq!(repository_summary_string(&["nas"]), "nas");
        assert_eq!(
            repository_summary_string(&["nas", "offsite"]),
            "nas, offsite"
        );
        // Overflow: cap near a kubectl column width with a +N marker.
        let long: Vec<String> = (0..8).map(|i| format!("repository-target-{i}")).collect();
        let refs: Vec<&str> = long.iter().map(String::as_str).collect();
        let summary = repository_summary_string(&refs);
        assert!(summary.len() <= 63, "{} chars: {summary}", summary.len());
        assert!(
            summary.starts_with("repository-target-0"),
            "spec order preserved: {summary}"
        );
        assert!(summary.contains(" +"), "overflow marker present: {summary}");
        // The names shown + the +N count always account for every repo.
        let shown = summary.split(" +").next().unwrap().split(", ").count();
        let more: usize = summary.rsplit(" +").next().unwrap().parse().unwrap();
        assert_eq!(shown + more, 8);
        // Deterministic.
        assert_eq!(summary, repository_summary_string(&refs));
    }

    // --- M10: the RepositoriesReady gate conditions ------------------------

    #[test]
    fn policy_ready_conditions_write_and_clear_the_registered_gate() {
        use kopiur_api::gates::POLICY_REPOSITORY_NOT_READY_GATE;
        let generation = Some(3);
        // Not ready: the gate is written FROM the registry row (blocked
        // polarity False, reason RepositoryNotReady) and Ready=False/
        // Reconciling=True carry the same reason — one truth.
        let msg = policy_repo_gate_message(&["Repository/apps/repo-b".to_string()]);
        let blocked = policy_ready_conditions(&[], generation, false, &msg);
        let gate = blocked
            .iter()
            .find(|c| c.type_ == crate::consts::REPOSITORIES_READY_CONDITION)
            .expect("gate condition written");
        assert!(POLICY_REPOSITORY_NOT_READY_GATE.matches(&gate.type_, &gate.status, &gate.reason));
        assert!(gate.message.contains("Repository/apps/repo-b"));
        let ready_cond = blocked
            .iter()
            .find(|c| c.type_ == crate::consts::READY_CONDITION)
            .unwrap();
        assert_eq!(ready_cond.status, "False");
        assert_eq!(
            ready_cond.reason,
            crate::consts::REPOSITORY_NOT_READY_REASON
        );

        // Recovery: the gate self-clears to True (because it exists) and Ready
        // returns.
        let healed = policy_ready_conditions(&blocked, generation, true, "");
        let gate = healed
            .iter()
            .find(|c| c.type_ == crate::consts::REPOSITORIES_READY_CONDITION)
            .expect("gate cleared, not dropped");
        assert_eq!(gate.status, "True");
        assert!(
            !POLICY_REPOSITORY_NOT_READY_GATE.trips(&gate.type_, &gate.status),
            "a cleared gate must not read as blocked"
        );

        // A never-gated policy never grows the condition: the healthy
        // single-repo status wire stays byte-identical.
        let never_gated = policy_ready_conditions(&[], generation, true, "");
        assert!(
            !never_gated
                .iter()
                .any(|c| c.type_ == crate::consts::REPOSITORIES_READY_CONDITION),
            "healthy-always policies must not grow the gate condition"
        );
    }

    // --- M10: per-repo #168 verification-gate input -------------------------

    #[test]
    fn multi_repo_has_success_matches_pins_only() {
        let rows = vec![
            pin(succeeded_backup("a1", at(2026, 5, 1)), "repo-a"),
            pin(failed_backup("b1", at(2026, 5, 1)), "repo-b"),
            succeeded_backup("legacy", at(2026, 5, 1)),
        ];
        assert!(multi_repo_has_success(&rows, "Repository/apps/repo-a"));
        assert!(
            !multi_repo_has_success(&rows, "Repository/apps/repo-b"),
            "a failed run is not a verifiable snapshot"
        );
        // Unpinned successes count for NO repo (their repository is not
        // knowable from the spec; the backfill pass pins them promptly).
        assert!(!multi_repo_has_success(&rows, "Repository/apps/repo-c"));
    }
}
