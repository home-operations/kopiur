//! Periodic sweep for **LEGACY per-run work-spec ConfigMaps** (#224).
//!
//! Current operator versions embed the mover work spec in the Job's pod env —
//! a run is exactly ONE object, cleaned up by its `ttlSecondsAfterFinished`,
//! and no per-run ConfigMap exists at all. Earlier versions instead mounted
//! the spec from a sidecar ConfigMap owner-referenced to a long-lived CR
//! (`Snapshot`/`SnapshotPolicy`/…) with no delete path: the Job self-reaped
//! via its TTL, the ConfigMap had no TTL mechanism, and one accumulated per
//! run, forever — clusters held hundreds (one per historical backup).
//!
//! This module heals upgraded clusters: a leader-only background task
//! periodically lists the kopiur-managed ConfigMaps and deletes the stale
//! work-spec ones. The decision kernel ([`sweep_candidates`]) is pure and
//! conservative — a ConfigMap is only an orphan when ALL of these hold:
//!
//! 1. labelled `app.kubernetes.io/managed-by=kopiur` (server-side pre-filtered,
//!    re-checked here),
//! 2. its `data` carries the work-spec key ([`crate::jobs::WORK_SPEC_FILE`]) —
//!    positively identifying a mover work-spec against every other
//!    kopiur-managed ConfigMap (projected creds, dashboards, …),
//! 3. its `data` does NOT carry a bootstrap result
//!    ([`kopiur_mover::bootstrap::RESULT_CONFIGMAP_KEY`]) — the bootstrap/probe
//!    flow writes `result.json` into its work-spec ConfigMap and consumes it
//!    after the Job is gone; racing that read could make a completed bootstrap
//!    look result-less,
//! 4. no same-named Job exists in its namespace — a pending/running run always
//!    has its Job (the Job is applied immediately after the ConfigMap), and
//! 5. it is older than `min_age_secs` — closing the ConfigMap-applied-before-Job
//!    window and any controller-crash-mid-spawn gap.
//!
//! The CLI's browse-session ConfigMap is additionally safe by construction: it
//! is owned by its session Job, so it either has a live Job (guard 4) or is
//! already being cascade-deleted with it.
//!
//! # Legacy per-run projected credential Secrets (#231)
//!
//! The same structural bug had a sibling: pre-#231 versions named projected
//! credential copies after the per-run mover Job (`{job}-creds-{idx}`), so
//! every recurring run (Maintenance, verification) minted a NEW Secret owned
//! by its long-lived CR — accumulating live credential copies forever. Current
//! versions use a stable per-CR name (refreshed in place; marked with
//! [`crate::consts::CREDS_SCOPE_LABEL`]). The same sweep pass heals upgraded
//! clusters via a second conservative kernel ([`legacy_creds_candidates`]):
//! a Secret is a legacy copy only when ALL of these hold:
//!
//! 1. labelled `app.kubernetes.io/managed-by=kopiur` AND
//!    `app.kubernetes.io/component=credentials` (server-side pre-filtered,
//!    re-checked),
//! 2. it carries [`crate::consts::PROJECTED_FROM_ANNOTATION`] — positively a
//!    projected copy (spares the kopia-UI credential mirrors and any user
//!    Secret),
//! 3. it does NOT carry the [`crate::consts::CREDS_SCOPE_LABEL`] marker — the
//!    marker identifies stable-named copies, which belong to the third kernel
//!    below,
//! 4. its name parses as `{job}-creds-<digits>` (the per-run copy shape) and
//!    no live kopiur-managed Job in its namespace still loads it via `envFrom`
//!    — an exact in-use test read from the listed Jobs' pod templates, which
//!    covers movers whose Job name differs from the copy's prefix (a populate
//!    Restore's Job `{restore}-populate` loads `{restore}-creds-0`), and
//! 5. it is older than `min_age_secs` (never below [`CREDS_MIN_AGE_FLOOR_SECS`]).
//!
//! # Stable copies of finished Snapshots (#240)
//!
//! A stable per-CR name bounds the copy count per CR — but a `Snapshot` is
//! *itself* the per-run object, so `{snapshot}-creds-0` is still one live copy of
//! the repository credentials per backup, owner-ref'd to a CR that outlives its
//! mover Job by the entire retention window. #234 made the name stable and the
//! leak survived it untouched: the fix is lifetime, not naming. A copy's real life
//! is "while some mover Job can still load it via `envFrom`".
//!
//! [`terminal_creds_candidates`] is the steady-state reaper (the consuming CR's
//! reconciler gets there first; this is the backstop, and what heals the backlog on
//! upgrade). It is the exact complement of [`legacy_creds_candidates`] — that kernel
//! owns the marker-LESS copies, this one the marker-BEARING copies, and a unit test
//! pins that they partition. A stable copy is reapable only when it carries the
//! marker AND the projected-from annotation AND a controller ownerRef to a
//! `Snapshot` of our API group that we positively observed **terminal** AND no live
//! Job loads it AND it is older than the floor.
//!
//! Keying on the owner's *phase* rather than on "no Job loads it" is deliberate
//! twice over. It keeps the kernel off the copies of *long-lived* CRs (a
//! `Maintenance` cron, a `SnapshotPolicy` verification tier), which idle between
//! slots with no Job for hours or weeks — reaping those would churn and would leave
//! the swept counter permanently non-zero, destroying the very signal that says this
//! leak is shut. And it is safe for a run still in flight, whose creds are projected
//! before its hooks and CSI staging, either of which may requeue for a long time
//! before a Job exists at all.
//!
//! Deleting Secrets needs the `secrets` delete verb, which the chart grants
//! under `features.credentialProjection.enabled`. A 403 (the flag was turned
//! off after projection was used) degrades to a warning naming the flag.

use std::collections::HashSet;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::api::{DeleteParams, ListParams, Preconditions};
use kube::core::{DynamicObject, PartialObjectMeta};
use kube::{Api, Resource, ResourceExt};

use kopiur_api::snapshot::{Snapshot, SnapshotPhase};
use kopiur_mover::bootstrap::RESULT_CONFIGMAP_KEY;

use crate::config;
use crate::consts::{
    COMPONENT_LABEL, CREDS_COMPONENT, CREDS_SCOPE_CR, CREDS_SCOPE_LABEL, MANAGED_BY_LABEL,
    MANAGED_BY_VALUE, OP_LABEL, OP_SNAPSHOT_DELETE_BATCH, PROJECTED_AT_ANNOTATION,
    PROJECTED_FROM_ANNOTATION, SNAPSHOT_CLEANUP_FINALIZER,
};
use crate::controllers::scoped_api;
use crate::error::Result;
use crate::jobs::WORK_SPEC_FILE;
use crate::metrics::Metrics;

/// Metadata-only view of a credential Secret. Every guard in both creds kernels —
/// and [`delete_with_preconditions`] itself — reads only `ObjectMeta`, so the sweep
/// never needs a Secret's `.data`. Listing these instead of full `Secret`s keeps
/// hundreds of base64 `KOPIA_PASSWORD` / `AWS_SECRET_ACCESS_KEY` blobs off the wire
/// and out of the operator's heap on every pass, the same reason
/// [`crate::controllers`] watches referents as `PartialObjectMeta`.
type SecretMeta = PartialObjectMeta<Secret>;

/// Floor (seconds) under which the creds kernels will not reap, regardless of the
/// operator's configured sweep min-age.
///
/// `min_age_secs` is a **shared** knob whose documented subject is the work-spec
/// ConfigMap, and it legally accepts `0`. An admin who zeroes it to drain
/// ConfigMaps faster must not thereby hand the creds kernels a zero-width guard on
/// the project→create-Job window, where a copy legitimately exists with no Job yet
/// (creds resolve at the top of the Snapshot reconcile; hooks and CSI staging can
/// sit between that and the Job).
pub const CREDS_MIN_AGE_FLOOR_SECS: i64 = 15 * 60;

/// Never sweep a shared `VolumeGroupSnapshot` younger than this, no matter how
/// aggressive the configured sweep interval.
///
/// The group is applied by whichever member reaches staging first, and its
/// siblings may not exist yet — so for a window there is a real group that no
/// live `Snapshot` references. Without this floor a sweep landing in that
/// window would delete a capture N backups are about to restore from.
pub const GROUP_MIN_AGE_FLOOR_SECS: i64 = 15 * 60;

/// Select the orphaned work-spec ConfigMaps out of `cms` (see the module doc
/// for the guard set). `live_jobs` is the `(namespace, name)` set of every
/// kopiur-managed Job; `now_unix` is the injected decision clock (unix seconds)
/// so the kernel unit-tests without a wall clock, mirroring
/// [`crate::io::classify_wedged_pods`].
pub fn sweep_candidates<'a>(
    cms: &'a [ConfigMap],
    live_jobs: &HashSet<(String, String)>,
    min_age_secs: i64,
    now_unix: i64,
) -> Vec<&'a ConfigMap> {
    cms.iter()
        .filter(|cm| {
            let managed = cm.metadata.labels.as_ref().is_some_and(|l| {
                l.get(MANAGED_BY_LABEL).map(String::as_str) == Some(MANAGED_BY_VALUE)
            });
            let Some(data) = cm.data.as_ref() else {
                return false;
            };
            let is_work_spec =
                data.contains_key(WORK_SPEC_FILE) && !data.contains_key(RESULT_CONFIGMAP_KEY);
            let key = (cm.namespace().unwrap_or_default(), cm.name_any());
            let age_secs = cm
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| now_unix - t.0.as_second())
                // No creationTimestamp (only possible for hand-built test
                // objects; the apiserver always stamps one) → treat as brand
                // new, never eligible.
                .unwrap_or(0);
            managed && is_work_spec && !live_jobs.contains(&key) && age_secs >= min_age_secs
        })
        .collect()
}

/// The mover-Job name a legacy projected credential Secret was minted for —
/// its name minus a trailing `-creds-<digits>` — or `None` when the name does
/// not match the projected-copy shape at all. The sweep uses it purely as the
/// shape guard; in-use protection comes from the Jobs' actual `envFrom` refs.
pub fn legacy_creds_source_job(secret_name: &str) -> Option<&str> {
    let (job, idx) = secret_name.rsplit_once("-creds-")?;
    (!job.is_empty() && !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit())).then_some(job)
}

/// Select the legacy per-run projected credential Secrets out of `secrets`
/// (see the module doc for the guard set). `live_creds_refs` is the
/// `(namespace, secret-name)` set of every credential Secret a live
/// kopiur-managed Job still loads via `envFrom` (see
/// [`job_env_from_secret_names`]) — the exact in-use test. `now_unix` is the
/// injected decision clock, mirroring [`sweep_candidates`].
pub fn legacy_creds_candidates<'a>(
    secrets: &'a [SecretMeta],
    live_creds_refs: &HashSet<(String, String)>,
    min_age_secs: i64,
    now_unix: i64,
) -> Vec<&'a SecretMeta> {
    secrets
        .iter()
        .filter(|s| is_legacy_creds_candidate(s, live_creds_refs, min_age_secs, now_unix))
        .collect()
}

/// Select the **stable per-CR** projected credential copies whose owning `Snapshot`
/// has finished — the steady-state creds reaper (#240).
///
/// This is the complement of [`legacy_creds_candidates`]: that kernel owns the
/// marker-LESS pre-#231 copies, this one owns the marker-BEARING copies. The two
/// partition the projected-creds population; neither ever selects the other's.
///
/// `terminal_snapshots` is the uid set of every `Snapshot` in a terminal phase.
/// Owning that set — rather than testing "no live Job loads it", which would be
/// simpler — is what keeps the kernel off the copies belonging to *long-lived* CRs
/// (`Maintenance`, `SnapshotPolicy` verification). Those sit idle between cron slots
/// with no live Job for hours, or weeks for a monthly verification tier; reaping
/// them would churn (delete → next run re-projects → delete) and would leave the
/// swept counter permanently non-zero, destroying the one signal that says this leak
/// is closed. Their copies are already bounded 1:1 by #234 and are not a leak.
///
/// It is also fail-CLOSED on the owner: a copy is reaped only when its owner is a
/// `Snapshot` we positively found *and* observed terminal. A partial or failed
/// Snapshot list therefore reaps nothing, rather than mistaking every owner for
/// "gone". Genuinely orphaned copies need no help from us — the ownerRef is valid
/// and same-namespace, so Kubernetes GC reaps them when the owner dies.
pub fn terminal_creds_candidates<'a>(
    secrets: &'a [SecretMeta],
    terminal_snapshots: &HashSet<String>,
    live_creds_refs: &HashSet<(String, String)>,
    min_age_secs: i64,
    now_unix: i64,
) -> Vec<&'a SecretMeta> {
    secrets
        .iter()
        .filter(|s| {
            is_terminal_creds_candidate(
                s,
                terminal_snapshots,
                live_creds_refs,
                min_age_secs,
                now_unix,
            )
        })
        .collect()
}

/// Whether ONE stable copy is reapable (see [`terminal_creds_candidates`]). Early
/// returns keep each guard independently readable.
fn is_terminal_creds_candidate(
    s: &SecretMeta,
    terminal_snapshots: &HashSet<String>,
    live_creds_refs: &HashSet<(String, String)>,
    min_age_secs: i64,
    now_unix: i64,
) -> bool {
    let Some(labels) = s.metadata.labels.as_ref() else {
        return false;
    };
    if labels.get(MANAGED_BY_LABEL).map(String::as_str) != Some(MANAGED_BY_VALUE)
        || labels.get(COMPONENT_LABEL).map(String::as_str) != Some(CREDS_COMPONENT)
        // The marker is REQUIRED here — a marker-less copy is the legacy kernel's.
        || labels.get(CREDS_SCOPE_LABEL).map(String::as_str) != Some(CREDS_SCOPE_CR)
    {
        return false;
    }
    // Positively a projected copy (spares the kopia-UI credential mirrors and any
    // user Secret that happens to carry our labels).
    if !s
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(PROJECTED_FROM_ANNOTATION))
    {
        return false;
    }
    // Controller-owned by a Snapshot of OUR api group that has finished. A copy
    // owned by anything else (Maintenance, SnapshotPolicy, Restore) is not ours.
    let owned_by_terminal_snapshot = s.metadata.owner_references.as_ref().is_some_and(|refs| {
        refs.iter().any(|o| {
            o.controller == Some(true)
                && o.kind == "Snapshot"
                && o.api_version
                    .split_once('/')
                    .is_some_and(|(group, _)| group == kopiur_api::GROUP)
                && terminal_snapshots.contains(&o.uid)
        })
    });
    if !owned_by_terminal_snapshot {
        return false;
    }
    // The in-use test: any live kopiur Job still loading it via `envFrom`. The
    // mover reads creds ONLY this way (never a volume mount), so this is exact.
    let key = (s.namespace().unwrap_or_default(), s.name_any());
    if live_creds_refs.contains(&key) {
        return false;
    }
    creds_age_secs(s, now_unix) >= min_age_secs.max(CREDS_MIN_AGE_FLOOR_SECS)
}

/// How long ago the copy was last written, in seconds.
///
/// Prefers [`PROJECTED_AT_ANNOTATION`], which the projection re-stamps on every run.
/// `creationTimestamp` cannot answer this: a stable copy is refreshed in place, so
/// its creation time never resets and a Secret re-projected one second ago still
/// looks a month old. Copies written before that annotation existed have none — they
/// fall back to `creationTimestamp`, which for them is correct (a legacy copy was
/// written exactly once) and, being genuinely ancient, keeps them eligible.
fn creds_age_secs(s: &SecretMeta, now_unix: i64) -> i64 {
    s.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(PROJECTED_AT_ANNOTATION))
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
        .map(|t| now_unix - t.timestamp())
        .or_else(|| {
            s.metadata
                .creation_timestamp
                .as_ref()
                .map(|t| now_unix - t.0.as_second())
        })
        // Neither stamp (only possible for hand-built objects; the apiserver always
        // stamps creationTimestamp) → treat as brand new, never eligible.
        .unwrap_or(0)
}

/// The uids of every `Snapshot` that has finished — the owners whose projected
/// credential copies are dead weight. A terminal `Snapshot` never mints another
/// backup mover (the reconciler's one-shot discipline), so nothing will re-read its
/// copy; the CR itself lives on for the whole retention window because it owns the
/// kopia snapshot, which is precisely why ownerRef GC cannot bound the copy.
fn terminal_snapshot_uids(snapshots: &[Snapshot]) -> HashSet<String> {
    snapshots
        .iter()
        .filter(|s| {
            matches!(
                s.status.as_ref().and_then(|st| st.phase),
                // `Unchanged` is terminal too — no further mover Job will ever
                // launch for it — so its projected credential copy is dead
                // weight the moment it lands. Omitting it leaks one Secret per
                // deduped run, forever, on exactly the workload this phase
                // exists for (a frequent schedule over static data): #240 all
                // over again.
                Some(SnapshotPhase::Succeeded)
                    | Some(SnapshotPhase::Failed)
                    | Some(SnapshotPhase::Unchanged)
            )
        })
        .filter_map(|s| s.uid())
        .collect()
}

// --- Batch delete-Job leak backstop (mass-deletion protection, M5a) ---------
//
// The batch dispatcher reaps its own terminal Jobs (succeeded once every member
// drained, failed after a back-off), but batch Jobs carry NO
// `ttlSecondsAfterFinished` — so a controller crash mid-reap, or a Job whose
// members all drained without a subsequent reconcile of the reaping CR, can
// leak a terminal Job forever. This backstop reaps the truly-orphaned ones. It
// is NOT the accounting path: the dispatcher owns the `kopiur_snapshot_delete_batch_jobs`
// counter (single increment at its reap), so this pass increments nothing and
// only logs — double-counting a Job the dispatcher already counted would corrupt
// the outcome series.

/// Page size for the sweep's full `Snapshot` read.
const SWEEP_LIST_PAGE_SIZE: u32 = 500;

/// LIST every `Snapshot` in scope, following `continue` tokens to completion.
///
/// This was a single unbounded, unpaginated, cluster-wide LIST — a full-object
/// dump of every Snapshot CR in the cluster in one response, 60s after every
/// controller start. During #319's restart storms that was fifteen extra
/// full-fleet dumps a day, landing in the tail of the startup watch burst.
///
/// Paginated rather than merely `.limit()`ed, because **completeness is
/// load-bearing here**: [`finalizer_holding_snapshot_uids`] SPARES batch delete
/// Jobs, so a silently truncated read would look like "no Snapshot holds a
/// finalizer" and reap Jobs that are still needed. A partial page is an error
/// that aborts the pass, exactly like a failed list.
async fn list_all_snapshots(api: &Api<Snapshot>) -> kube::Result<Vec<Snapshot>> {
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut lp = ListParams::default().limit(SWEEP_LIST_PAGE_SIZE);
        if let Some(t) = &token {
            lp = lp.continue_token(t);
        }
        let page = api.list(&lp).await?;
        token = page.metadata.continue_.clone().filter(|t| !t.is_empty());
        all.extend(page.items);
        if token.is_none() {
            return Ok(all);
        }
    }
}

/// The UIDs of `Snapshot`s (from the sweep's OWN list) still holding the cleanup
/// finalizer — the "delete still pending" set that SPARES a batch Job. Because the
/// sweep LISTs Snapshots directly (a complete read, not the possibly-cold reflector
/// store the dispatcher guards), the set is always trustworthy here; a failed list
/// aborts the whole pass upstream, never reaping on an empty read.
pub fn finalizer_holding_snapshot_uids(snapshots: &[Snapshot]) -> HashSet<String> {
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

/// Select the LEAKED terminal batch delete Jobs out of `jobs`: a Job is a
/// candidate only when ALL of these hold —
///
/// 1. labelled `managed-by=kopiur` AND the batch op (`op=snapshot-delete-batch`),
/// 2. TERMINAL ([`crate::snapshot::job_terminal_state`] `is_some`),
/// 3. older than `min_age_secs`, and
/// 4. NONE of its member UIDs ([`crate::snapshot::batch_job_members`]) still hold
///    the cleanup finalizer (`finalizer_holding`) — i.e. every member drained, so
///    the Job's work is fully consumed and it can only be a leak.
///
/// Guard 4 is the real protection: a still-held member means that member's delete
/// is pending (a succeeded Job not yet observed, or a failed Job about to re-fire),
/// so the sweep never races an in-flight retry. `now_unix` is the injected clock,
/// mirroring [`sweep_candidates`].
pub fn batch_job_sweep_candidates<'a>(
    jobs: &'a [Job],
    finalizer_holding: &HashSet<String>,
    min_age_secs: i64,
    now_unix: i64,
) -> Vec<&'a Job> {
    jobs.iter()
        .filter(|j| is_leaked_batch_job(j, finalizer_holding, min_age_secs, now_unix))
        .collect()
}

/// Whether ONE Job is a leaked terminal batch delete Job (see
/// [`batch_job_sweep_candidates`]). Early returns keep each guard readable.
fn is_leaked_batch_job(
    job: &Job,
    finalizer_holding: &HashSet<String>,
    min_age_secs: i64,
    now_unix: i64,
) -> bool {
    let labels = job.metadata.labels.as_ref();
    let has =
        |key: &str, val: &str| labels.is_some_and(|l| l.get(key).map(String::as_str) == Some(val));
    if !has(MANAGED_BY_LABEL, MANAGED_BY_VALUE) || !has(OP_LABEL, OP_SNAPSHOT_DELETE_BATCH) {
        return false;
    }
    // Only terminal Jobs leak; a live batch is the dispatcher's to poll.
    if crate::snapshot::job_terminal_state(job).is_none() {
        return false;
    }
    let members = crate::snapshot::batch_job_members(job);
    // A member still holding the finalizer means its delete is still pending —
    // never reap the Job out from under an in-flight (or retrying) member.
    if members.iter().any(|u| finalizer_holding.contains(u)) {
        return false;
    }
    let age_secs = job
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|t| now_unix - t.0.as_second())
        // No creationTimestamp (only hand-built test objects) → treat as new.
        .unwrap_or(0);
    age_secs >= min_age_secs
}

/// The Secret names a Job's pods load via `envFrom` (containers and
/// initContainers). Pure so the extraction is unit-testable.
pub fn job_env_from_secret_names(job: &Job) -> Vec<String> {
    let Some(pod) = job.spec.as_ref().map(|s| &s.template.spec) else {
        return Vec::new();
    };
    let Some(pod) = pod.as_ref() else {
        return Vec::new();
    };
    pod.containers
        .iter()
        .chain(pod.init_containers.iter().flatten())
        .flat_map(|c| c.env_from.iter().flatten())
        .filter_map(|e| e.secret_ref.as_ref().map(|r| r.name.clone()))
        .collect()
}

/// Whether ONE Secret passes every legacy-copy guard (module doc). Early
/// returns keep each guard independently readable.
fn is_legacy_creds_candidate(
    s: &SecretMeta,
    live_creds_refs: &HashSet<(String, String)>,
    min_age_secs: i64,
    now_unix: i64,
) -> bool {
    let Some(labels) = s.metadata.labels.as_ref() else {
        return false;
    };
    if labels.get(MANAGED_BY_LABEL).map(String::as_str) != Some(MANAGED_BY_VALUE)
        || labels.get(COMPONENT_LABEL).map(String::as_str) != Some(CREDS_COMPONENT)
        || labels.contains_key(CREDS_SCOPE_LABEL)
    {
        return false;
    }
    let projected = s
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(PROJECTED_FROM_ANNOTATION));
    if !projected {
        return false;
    }
    let name = s.name_any();
    if legacy_creds_source_job(&name).is_none() {
        return false;
    }
    let ns = s.namespace().unwrap_or_default();
    if live_creds_refs.contains(&(ns, name)) {
        return false;
    }
    creds_age_secs(s, now_unix) >= min_age_secs.max(CREDS_MIN_AGE_FLOOR_SECS)
}

/// What one guarded delete did (see [`delete_with_preconditions`]).
pub(crate) enum DeleteOutcome {
    /// The object was deleted.
    Deleted,
    /// 404 (already gone) or 409 (changed since classification — a live run
    /// reclaimed it): spared, re-evaluated next pass.
    Spared,
    /// 403: the operator lacks the delete verb for this resource.
    Forbidden,
}

/// Delete `name` pinned to the exact `uid`+`resourceVersion` the sweep
/// classified, tolerating the benign outcomes (module doc: the TOCTOU guard).
pub(crate) async fn delete_with_preconditions<K>(
    api: &Api<K>,
    name: &str,
    uid: Option<String>,
    resource_version: Option<String>,
) -> Result<DeleteOutcome>
where
    K: Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let dp = DeleteParams {
        preconditions: Some(Preconditions {
            uid,
            resource_version,
        }),
        ..DeleteParams::default()
    };
    match api.delete(name, &dp).await {
        Ok(_) => Ok(DeleteOutcome::Deleted),
        Err(kube::Error::Api(ae)) if ae.code == 404 || ae.code == 409 => Ok(DeleteOutcome::Spared),
        Err(kube::Error::Api(ae)) if ae.code == 403 => Ok(DeleteOutcome::Forbidden),
        Err(e) => Err(crate::error::Error::Kube(e)),
    }
}

/// What one sweep pass deleted, per victim kind — plus the live population it saw.
pub struct SweepOutcome {
    /// Orphaned mover work-spec ConfigMaps (#224).
    pub work_spec_cms: usize,
    /// Legacy per-run projected credential Secrets (#231).
    pub projected_secrets: usize,
    /// Stable per-CR copies whose owning `Snapshot` had finished (#240).
    pub terminal_creds_secrets: usize,
    /// Leaked TERMINAL batch delete Jobs whose members all drained (M5a). Logged,
    /// never metered — the dispatcher owns the batch-Job outcome counter.
    pub batch_jobs: usize,
    /// Leaked shared `VolumeGroupSnapshot`s whose members are all gone (#346).
    pub group_snapshots: usize,
    /// Shared `VolumeGroupSnapshot`s alive at classification time, BEFORE this
    /// pass's deletes. Backs the live gauge: a delete counter alone cannot tell
    /// a healthy fleet from one quietly accumulating captures, which is exactly
    /// how the projected-Secret leak ran unseen for weeks.
    pub group_snapshots_live: usize,
    /// Projected credential Secrets alive at classification time, BEFORE this
    /// pass's deletes. Backs the `kopiur_projected_secrets` gauge — the population
    /// signal that was missing, and the reason this leak ran for weeks unseen: a
    /// counter of projections rises identically whether or not anything leaks.
    pub projected_secrets_live: usize,
}

/// One sweep pass: list the kopiur-managed ConfigMaps, projected credential
/// Secrets, and Jobs within the install scope, select the orphans with the two
/// pure kernels, and delete them (per-item errors are logged and skipped —
/// degrade, don't crash). Returns the counts deleted.
///
/// Three guards close the classify→delete TOCTOU window for every victim kind:
/// - ConfigMaps and Secrets are listed BEFORE Jobs: a run spawned between the
///   lists shows up only in the Job list (keeping its objects), never the
///   reverse.
/// - Each delete carries a `resourceVersion` PRECONDITION pinned to the exact
///   object version the sweep classified. A run re-spawned during the delete loop
///   server-side-applies the SAME-NAMED object (same UID, same old
///   creationTimestamp — min-age can't help), and because the projection always
///   re-stamps [`PROJECTED_AT_ANNOTATION`] that write is never a no-op, so it
///   bumps the resourceVersion, the delete fails 409, and the live run is spared.
///   (Without that stamp the re-apply would be byte-identical, the apiserver would
///   skip the write, the resourceVersion would not move, and this precondition
///   would silently pass — deleting a Secret a live Job is about to load.)
/// - Snapshots are listed and only TERMINAL ones admit their copies, so a copy
///   belonging to a run still in flight is never a candidate however long that run
///   sits in hooks or CSI staging without a Job.
pub async fn run_sweep(
    client: &kube::Client,
    scope: &config::WatchScope,
    min_age_secs: i64,
) -> Result<SweepOutcome> {
    let managed = format!("{MANAGED_BY_LABEL}={MANAGED_BY_VALUE}");
    let cm_api: Api<ConfigMap> = scoped_api(client, scope);
    let cms = cm_api
        .list(&ListParams::default().labels(&managed))
        .await?
        .items;
    // Metadata-only: the sweep never needs a credential's plaintext (see `SecretMeta`).
    let secret_api: Api<SecretMeta> = scoped_api(client, scope);
    let secrets = secret_api
        .list(
            &ListParams::default()
                .labels(&format!("{managed},{COMPONENT_LABEL}={CREDS_COMPONENT}")),
        )
        .await?
        .items;
    let snapshot_api: Api<Snapshot> = scoped_api(client, scope);
    let snapshots = list_all_snapshots(&snapshot_api).await?;
    let job_api: Api<Job> = scoped_api(client, scope);
    let jobs = job_api
        .list(&ListParams::default().labels(&managed))
        .await?
        .items;
    let live_set: HashSet<(String, String)> = jobs
        .iter()
        .map(|j| (j.namespace().unwrap_or_default(), j.name_any()))
        .collect();
    let live_creds_refs: HashSet<(String, String)> = jobs
        .iter()
        .flat_map(|j| {
            let ns = j.namespace().unwrap_or_default();
            job_env_from_secret_names(j)
                .into_iter()
                .map(move |name| (ns.clone(), name))
        })
        .collect();
    let terminal = terminal_snapshot_uids(&snapshots);
    let finalizer_holding = finalizer_holding_snapshot_uids(&snapshots);
    let now = chrono::Utc::now().timestamp();

    // Shared VolumeGroupSnapshots. They deliberately carry NO ownerReferences
    // (see `io::group_staging`), so ownerRef GC can never reclaim one — if the
    // controller dies between the last member finishing and the reap, the
    // capture is immortal. This is that backstop.
    let group_api: Api<DynamicObject> = match scope {
        config::WatchScope::Cluster => Api::all_with(
            client.clone(),
            &crate::io::group_staging::volume_group_snapshot_ar(),
        ),
        config::WatchScope::Namespaced(ns) => Api::namespaced_with(
            client.clone(),
            ns,
            &crate::io::group_staging::volume_group_snapshot_ar(),
        ),
    };
    // A cluster without the group-snapshot API group (404) is the common case,
    // not an error: most clusters do not serve this Beta group.
    let groups = match group_api
        .list(&ListParams::default().labels(&managed))
        .await
    {
        Ok(list) => list.items,
        Err(kube::Error::Api(e)) if e.code == 404 => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let live_groups: HashSet<(String, String)> = snapshots
        .iter()
        .filter_map(|s| {
            let g = s.spec.source.as_ref()?.group.as_ref()?;
            Some((g.namespace.clone(), g.volume_group_snapshot_name.clone()))
        })
        .collect();

    Ok(SweepOutcome {
        projected_secrets_live: secrets.len(),
        group_snapshots_live: groups.len(),
        group_snapshots: delete_group_snapshot_victims(
            client,
            group_snapshot_candidates(&groups, &live_groups, min_age_secs, now),
        )
        .await,
        work_spec_cms: delete_cm_victims(
            client,
            sweep_candidates(&cms, &live_set, min_age_secs, now),
        )
        .await,
        projected_secrets: delete_secret_victims(
            client,
            legacy_creds_candidates(&secrets, &live_creds_refs, min_age_secs, now),
            "legacy per-run",
        )
        .await,
        terminal_creds_secrets: delete_secret_victims(
            client,
            terminal_creds_candidates(&secrets, &terminal, &live_creds_refs, min_age_secs, now),
            "finished-Snapshot",
        )
        .await,
        batch_jobs: delete_batch_job_victims(
            client,
            batch_job_sweep_candidates(&jobs, &finalizer_holding, min_age_secs, now),
        )
        .await,
    })
}

/// **Pure.** The leaked shared `VolumeGroupSnapshot`s: kopiur-managed, older
/// than the min-age floor, and named by NO live `Snapshot`'s
/// `spec.source.group`.
///
/// The min-age floor is what makes this safe against the create race: a group
/// is applied by whichever member reaches staging first, and its siblings may
/// not have been created yet. Without the floor a sweep landing in that window
/// would delete a capture that is about to be needed.
pub fn group_snapshot_candidates<'a>(
    groups: &'a [DynamicObject],
    live_groups: &HashSet<(String, String)>,
    min_age_secs: i64,
    now_unix: i64,
) -> Vec<&'a DynamicObject> {
    let floor = min_age_secs.max(GROUP_MIN_AGE_FLOOR_SECS);
    groups
        .iter()
        .filter(|g| {
            let key = (g.namespace().unwrap_or_default(), g.name_any());
            if live_groups.contains(&key) {
                return false;
            }
            g.metadata
                .creation_timestamp
                .as_ref()
                .map(|t| now_unix - t.0.as_second() >= floor)
                // No creationTimestamp: a partially-read object. Leave it —
                // this backstop must never be the thing that loses a capture.
                .unwrap_or(false)
        })
        .collect()
}

/// Delete the leaked shared groups (background propagation, so their member
/// VolumeSnapshots go too), tolerating a 404.
async fn delete_group_snapshot_victims(
    client: &kube::Client,
    victims: Vec<&DynamicObject>,
) -> usize {
    let mut deleted = 0;
    for g in victims {
        let ns = g.namespace().unwrap_or_default();
        let name = g.name_any();
        let api: Api<DynamicObject> = Api::namespaced_with(
            client.clone(),
            &ns,
            &crate::io::group_staging::volume_group_snapshot_ar(),
        );
        match api.delete(&name, &DeleteParams::background()).await {
            Ok(_) => {
                tracing::info!(namespace = %ns, name = %name, "swept leaked VolumeGroupSnapshot (no live Snapshot references it)");
                deleted += 1;
            }
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => {
                tracing::warn!(namespace = %ns, name = %name, error = %e, "could not sweep leaked VolumeGroupSnapshot");
            }
        }
    }
    deleted
}

/// Delete the leaked terminal batch delete Jobs (background propagation, so their
/// pods go too), tolerating a 404. Per-item failures warn and skip; NO metric —
/// the dispatcher is the accounting path (module doc).
async fn delete_batch_job_victims(client: &kube::Client, victims: Vec<&Job>) -> usize {
    let mut deleted = 0;
    for job in victims {
        let ns = job.namespace().unwrap_or_default();
        let name = job.name_any();
        let api: Api<Job> = Api::namespaced(client.clone(), &ns);
        match api.delete(&name, &DeleteParams::background()).await {
            Ok(_) => {
                deleted += 1;
                tracing::info!(job = %name, namespace = %ns,
                    "reaped leaked terminal batch delete Job (members all drained)");
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}
            Err(e) => {
                tracing::warn!(job = %name, namespace = %ns, error = %e,
                    "leaked batch delete Job reap failed (skipped)");
            }
        }
    }
    deleted
}

/// Delete the classified work-spec ConfigMap orphans; returns the count
/// deleted. Per-item failures warn and skip (degrade, don't crash).
async fn delete_cm_victims(client: &kube::Client, victims: Vec<&ConfigMap>) -> usize {
    let mut deleted = 0;
    for cm in victims {
        let ns = cm.namespace().unwrap_or_default();
        let name = cm.name_any();
        let api: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
        match delete_with_preconditions(&api, &name, cm.uid(), cm.resource_version()).await {
            Ok(DeleteOutcome::Deleted) => deleted += 1,
            Ok(DeleteOutcome::Spared) => {}
            // ConfigMap delete is an unconditional chart grant; a 403 here
            // means a hand-trimmed Role — surface it like any other failure.
            Ok(DeleteOutcome::Forbidden) => {
                tracing::warn!(configmap = %name, namespace = %ns,
                    "orphaned work-spec ConfigMap delete forbidden (skipped)");
            }
            Err(e) => {
                tracing::warn!(configmap = %name, namespace = %ns, error = %e,
                    "orphaned work-spec ConfigMap delete failed (skipped)");
            }
        }
    }
    deleted
}

/// Delete the classified projected-credentials Secrets; returns the count deleted.
/// `kind` names the classification for the logs (both creds kernels feed this). A
/// 403 names the Helm flag whose grant carries the delete verb (degrade, don't
/// crash).
async fn delete_secret_victims(
    client: &kube::Client,
    victims: Vec<&SecretMeta>,
    kind: &str,
) -> usize {
    let mut deleted = 0;
    for s in victims {
        let ns = s.namespace().unwrap_or_default();
        let name = s.name_any();
        let api: Api<Secret> = Api::namespaced(client.clone(), &ns);
        match delete_with_preconditions(&api, &name, s.uid(), s.resource_version()).await {
            Ok(DeleteOutcome::Deleted) => {
                deleted += 1;
                tracing::info!(secret = %name, namespace = %ns, kind,
                    "reaped projected credentials Secret");
            }
            Ok(DeleteOutcome::Spared) => {}
            Ok(DeleteOutcome::Forbidden) => {
                tracing::warn!(secret = %name, namespace = %ns, kind,
                    flag = crate::consts::CREDENTIAL_PROJECTION_FLAG,
                    "projected credentials Secret delete forbidden: the operator \
                     lacks the `secrets` delete verb (the credentialProjection flag was \
                     likely disabled after projection was used). Re-enable the flag or \
                     delete the copies by hand (skipped)");
            }
            Err(e) => {
                tracing::warn!(secret = %name, namespace = %ns, kind, error = %e,
                    "projected credentials Secret delete failed (skipped)");
            }
        }
    }
    deleted
}

/// Spawn the periodic sweep as a background task. Call ONLY after leader
/// election is won (a single writer — mirroring the reconcilers). The first
/// pass runs shortly after startup so upgraded clusters heal promptly; later
/// passes run every `interval_secs`. `interval_secs == 0` disables the sweep
/// entirely (the config layer documents the knob).
pub fn spawn_sweep(
    client: kube::Client,
    scope: config::WatchScope,
    metrics: Metrics,
    interval_secs: u64,
    min_age_secs: i64,
) {
    if interval_secs == 0 {
        tracing::info!("orphaned-object sweep disabled (interval 0)");
        return;
    }
    tokio::spawn(async move {
        // Short initial delay: let the watches/caches warm up and avoid piling
        // the sweep's LISTs onto the startup burst.
        let mut delay = std::time::Duration::from_secs(60);
        loop {
            tokio::time::sleep(delay).await;
            delay = std::time::Duration::from_secs(interval_secs);
            match run_sweep(&client, &scope, min_age_secs).await {
                Ok(outcome) => {
                    if outcome.work_spec_cms > 0 {
                        metrics.inc_work_spec_cms_swept(outcome.work_spec_cms as u64);
                        tracing::info!(
                            deleted = outcome.work_spec_cms,
                            "swept orphaned work-spec ConfigMaps"
                        );
                    }
                    if outcome.projected_secrets > 0 {
                        metrics.inc_projected_secrets_swept(outcome.projected_secrets as u64);
                        tracing::info!(
                            deleted = outcome.projected_secrets,
                            "swept legacy per-run projected credential Secrets"
                        );
                    }
                    if outcome.terminal_creds_secrets > 0 {
                        metrics.inc_creds_secrets_reaped(
                            "sweep",
                            outcome.terminal_creds_secrets as u64,
                        );
                        tracing::info!(
                            deleted = outcome.terminal_creds_secrets,
                            "reaped projected credential Secrets of finished Snapshots"
                        );
                    }
                    // Leak backstop only — NOT metered (the dispatcher owns the
                    // batch-Job outcome counter; a second increment here would
                    // double-count). Log so a persistent leak is still visible.
                    if outcome.batch_jobs > 0 {
                        tracing::info!(
                            deleted = outcome.batch_jobs,
                            "reaped leaked terminal batch delete Jobs (dispatcher backstop)"
                        );
                    }
                    // The population gauge, not just the deltas: a rising gauge is
                    // the regression alarm this leak had no way to trip.
                    metrics.set_projected_secrets_live(
                        outcome.projected_secrets_live.saturating_sub(
                            outcome.projected_secrets + outcome.terminal_creds_secrets,
                        ) as i64,
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "orphaned-object sweep failed; will retry next interval");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{OwnerReference, Time};
    use kube::core::ObjectMeta;
    use std::collections::BTreeMap;

    const NOW: i64 = 1_700_000_000;
    const HOUR: i64 = 3600;
    const DAY: i64 = 24 * HOUR;

    /// A kopiur-managed ConfigMap in `ns` created `age_secs` ago whose data
    /// holds exactly `keys`.
    fn cm(ns: &str, name: &str, age_secs: i64, keys: &[&str], managed: bool) -> ConfigMap {
        let mut labels = BTreeMap::new();
        if managed {
            labels.insert(MANAGED_BY_LABEL.to_string(), MANAGED_BY_VALUE.to_string());
        }
        ConfigMap {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(ns.to_string()),
                labels: Some(labels),
                creation_timestamp: Some(Time(
                    k8s_openapi::jiff::Timestamp::from_second(NOW - age_secs).unwrap(),
                )),
                ..Default::default()
            },
            data: Some(
                keys.iter()
                    .map(|k| (k.to_string(), "{}".to_string()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    fn names(picked: &[&ConfigMap]) -> Vec<String> {
        picked.iter().map(|c| c.name_any()).collect()
    }

    #[test]
    fn old_orphaned_work_spec_cm_is_selected() {
        let cms = vec![cm(
            "media",
            "qui-20260708155044",
            2 * HOUR,
            &[WORK_SPEC_FILE],
            true,
        )];
        let picked = sweep_candidates(&cms, &HashSet::new(), HOUR, NOW);
        assert_eq!(names(&picked), vec!["qui-20260708155044"]);
    }

    #[test]
    fn young_cm_is_skipped_until_min_age() {
        // Closes the ConfigMap-applied-before-Job window: a freshly-applied
        // work-spec whose Job hasn't landed yet must never be reaped.
        let cms = vec![cm("media", "qui-fresh", HOUR - 1, &[WORK_SPEC_FILE], true)];
        assert!(sweep_candidates(&cms, &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn cm_with_live_same_named_job_is_skipped() {
        let cms = vec![cm(
            "media",
            "qui-running",
            2 * HOUR,
            &[WORK_SPEC_FILE],
            true,
        )];
        let live: HashSet<_> = [("media".to_string(), "qui-running".to_string())].into();
        assert!(sweep_candidates(&cms, &live, HOUR, NOW).is_empty());
    }

    #[test]
    fn job_in_another_namespace_does_not_shield_a_same_named_cm() {
        // The live-Job guard keys on (namespace, name) — a Job named like the
        // ConfigMap but in a different namespace is a different run.
        let cms = vec![cm("media", "qui-x", 2 * HOUR, &[WORK_SPEC_FILE], true)];
        let live: HashSet<_> = [("automation".to_string(), "qui-x".to_string())].into();
        assert_eq!(
            names(&sweep_candidates(&cms, &live, HOUR, NOW)),
            vec!["qui-x"]
        );
    }

    #[test]
    fn non_work_spec_managed_cm_is_never_selected() {
        // kopiur manages other ConfigMaps (dashboards, …) under the same
        // label; only the work-spec data key marks a sweep target.
        let cms = vec![cm("media", "other", 2 * HOUR, &["dashboard.json"], true)];
        assert!(sweep_candidates(&cms, &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn bootstrap_result_cm_is_never_selected() {
        // The bootstrap/probe flow PATCHes result.json INTO its work-spec
        // ConfigMap and reads it back after the Job is gone — sweeping it
        // would make a completed bootstrap look result-less.
        let cms = vec![cm(
            "kopiur-system",
            "repo-discovery",
            2 * HOUR,
            &[WORK_SPEC_FILE, RESULT_CONFIGMAP_KEY],
            true,
        )];
        assert!(sweep_candidates(&cms, &HashSet::new(), HOUR, NOW).is_empty());
    }

    // --- legacy projected credential Secrets (#231) --------------------------

    use crate::consts::PROJECTED_FROM_ANNOTATION;
    use crate::consts::{COMPONENT_LABEL, CREDS_COMPONENT, CREDS_SCOPE_CR, CREDS_SCOPE_LABEL};

    /// A fully legacy-shaped projected credential Secret in `ns` created
    /// `age_secs` ago: managed-by + component labels, projected-from
    /// annotation, NO scope marker, NO ownerRef. Tests mutate it into the control
    /// shapes. Metadata-only, exactly as the sweep lists it.
    fn legacy_secret(ns: &str, name: &str, age_secs: i64) -> SecretMeta {
        let labels = BTreeMap::from([
            (MANAGED_BY_LABEL.to_string(), MANAGED_BY_VALUE.to_string()),
            (COMPONENT_LABEL.to_string(), CREDS_COMPONENT.to_string()),
        ]);
        let annotations = BTreeMap::from([(
            PROJECTED_FROM_ANNOTATION.to_string(),
            "kopiur-system/repo-pw".to_string(),
        )]);
        SecretMeta {
            types: None,
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(ns.to_string()),
                labels: Some(labels),
                annotations: Some(annotations),
                creation_timestamp: Some(Time(
                    k8s_openapi::jiff::Timestamp::from_second(NOW - age_secs).unwrap(),
                )),
                ..Default::default()
            },
            _phantom: std::marker::PhantomData,
        }
    }

    /// A stable per-CR copy: the legacy shape plus the scope marker and a
    /// controller ownerRef to a `Snapshot` of our API group — i.e. what
    /// `build_projected_secret` writes today. `age_secs` is applied to
    /// `projected-at` (the re-stamped clock), not `creationTimestamp`, mirroring a
    /// copy refreshed in place.
    fn stable_secret(ns: &str, name: &str, owner_uid: &str, age_secs: i64) -> SecretMeta {
        let mut s = legacy_secret(ns, name, 400 * DAY);
        s.metadata
            .labels
            .as_mut()
            .unwrap()
            .insert(CREDS_SCOPE_LABEL.to_string(), CREDS_SCOPE_CR.to_string());
        s.metadata.annotations.as_mut().unwrap().insert(
            PROJECTED_AT_ANNOTATION.to_string(),
            chrono::DateTime::from_timestamp(NOW - age_secs, 0)
                .unwrap()
                .to_rfc3339(),
        );
        s.metadata.owner_references =
            Some(vec![owner_ref("Snapshot", owner_uid, kopiur_api::GROUP)]);
        s
    }

    fn owner_ref(kind: &str, uid: &str, group: &str) -> OwnerReference {
        OwnerReference {
            api_version: format!("{group}/v1alpha1"),
            kind: kind.to_string(),
            name: "owner".to_string(),
            uid: uid.to_string(),
            controller: Some(true),
            block_owner_deletion: Some(false),
        }
    }

    fn secret_names(picked: &[&SecretMeta]) -> Vec<String> {
        picked.iter().map(|s| s.name_any()).collect()
    }

    fn uids(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn creds_refs(pairs: &[(&str, &str)]) -> HashSet<(String, String)> {
        pairs
            .iter()
            .map(|(ns, n)| (ns.to_string(), n.to_string()))
            .collect()
    }

    #[test]
    fn legacy_creds_source_job_parses_only_the_projected_shape() {
        assert_eq!(
            legacy_creds_source_job("app-q-1751831000-creds-0"),
            Some("app-q-1751831000")
        );
        assert_eq!(legacy_creds_source_job("a-creds-12"), Some("a"));
        assert_eq!(legacy_creds_source_job("foo-creds"), None);
        assert_eq!(legacy_creds_source_job("foo-creds-x"), None);
        assert_eq!(legacy_creds_source_job("foo-creds-"), None);
        assert_eq!(legacy_creds_source_job("-creds-0"), None);
        assert_eq!(legacy_creds_source_job("foocreds-0"), None);
    }

    #[test]
    fn aged_legacy_creds_copy_is_selected() {
        let secrets = vec![legacy_secret("media", "app-q-1751831000-creds-0", 2 * HOUR)];
        let picked = legacy_creds_candidates(&secrets, &HashSet::new(), HOUR, NOW);
        assert_eq!(secret_names(&picked), vec!["app-q-1751831000-creds-0"]);
    }

    #[test]
    fn marker_labeled_copy_is_never_selected_by_the_legacy_kernel() {
        // The scope marker routes a STABLE copy to the OTHER kernel — it does not
        // mean "never reaped" (it did before #240). The legacy kernel must not touch
        // one, because it has no way to tell a live consumer from a dead one: a
        // stable copy's creationTimestamp never resets on re-apply, so min-age is no
        // shield for it.
        let s = stable_secret("media", "app-maint-creds-0", "uid-1", 2 * HOUR);
        assert!(legacy_creds_candidates(&[s], &HashSet::new(), HOUR, NOW).is_empty());
    }

    // --- stable copies of finished Snapshots (#240) ---------------------------

    #[test]
    fn stable_copy_of_a_finished_snapshot_is_reaped() {
        let s = stable_secret("media", "app-20260712-creds-0", "uid-1", 2 * HOUR);
        let picked = terminal_creds_candidates(
            std::slice::from_ref(&s),
            &uids(&["uid-1"]),
            &HashSet::new(),
            HOUR,
            NOW,
        );
        assert_eq!(secret_names(&picked), vec!["app-20260712-creds-0"]);
    }

    #[test]
    fn stable_copy_of_a_running_snapshot_is_spared() {
        // The owner exists but has not finished: its mover Job may not even be born
        // yet (creds resolve before hooks and CSI staging, either of which can requeue
        // for a long time without a Job). Phase, not Job-absence, is the gate.
        let s = stable_secret("media", "app-20260712-creds-0", "uid-1", 2 * HOUR);
        assert!(
            terminal_creds_candidates(&[s], &HashSet::new(), &HashSet::new(), HOUR, NOW).is_empty(),
            "a copy whose owning Snapshot is not terminal must never be a candidate"
        );
    }

    #[test]
    fn copies_of_long_lived_owners_are_never_reaped_by_the_sweep() {
        // THE churn guard. A Maintenance cron's or a SnapshotPolicy verification's
        // copy sits idle between slots — hours, or weeks for a monthly tier — with no
        // live Job. A kernel keyed on "no Job loads it" would delete it every pass and
        // the next run would re-project it: endless churn, and a swept counter that
        // never settles to zero, destroying the one signal that says this leak is shut.
        // They are bounded 1:1 by #234 and are not a leak. Only Snapshot owners here.
        for kind in ["Maintenance", "SnapshotPolicy", "Restore"] {
            let mut s = stable_secret("media", "app-maint-creds-0", "uid-1", 30 * DAY);
            s.metadata.owner_references = Some(vec![owner_ref(kind, "uid-1", kopiur_api::GROUP)]);
            assert!(
                terminal_creds_candidates(&[s], &uids(&["uid-1"]), &HashSet::new(), HOUR, NOW)
                    .is_empty(),
                "a {kind}-owned copy must never be swept"
            );
        }
    }

    #[test]
    fn a_live_job_shields_a_finished_snapshots_copy() {
        // The mover stamps `Succeeded` from inside the pod, before the pod exits and
        // before the Job goes terminal — and with backoffLimit the Job can still
        // schedule a replacement pod that re-reads this very Secret via envFrom.
        // Terminal phase alone is NOT enough; the in-use test is the second gate.
        let s = stable_secret("media", "app-creds-0", "uid-1", 2 * HOUR);
        let live = creds_refs(&[("media", "app-creds-0")]);
        assert!(
            terminal_creds_candidates(
                std::slice::from_ref(&s),
                &uids(&["uid-1"]),
                &live,
                HOUR,
                NOW
            )
            .is_empty()
        );
    }

    #[test]
    fn a_foreign_or_unowned_copy_is_never_reaped() {
        let terminal = uids(&["uid-1"]);
        // Owned by a Snapshot in someone else's API group (a same-named CRD).
        let mut foreign = stable_secret("media", "app-creds-0", "uid-1", 2 * HOUR);
        foreign.metadata.owner_references = Some(vec![owner_ref(
            "Snapshot",
            "uid-1",
            "snapshot.storage.k8s.io",
        )]);
        assert!(
            terminal_creds_candidates(&[foreign], &terminal, &HashSet::new(), HOUR, NOW).is_empty()
        );
        // No ownerRef at all (a user-crafted Secret wearing our labels).
        let mut orphan = stable_secret("media", "app-creds-0", "uid-1", 2 * HOUR);
        orphan.metadata.owner_references = None;
        assert!(
            terminal_creds_candidates(&[orphan], &terminal, &HashSet::new(), HOUR, NOW).is_empty()
        );
        // Not the controller ownerRef.
        let mut not_controller = stable_secret("media", "app-creds-0", "uid-1", 2 * HOUR);
        not_controller.metadata.owner_references.as_mut().unwrap()[0].controller = Some(false);
        assert!(
            terminal_creds_candidates(&[not_controller], &terminal, &HashSet::new(), HOUR, NOW)
                .is_empty()
        );
    }

    #[test]
    fn a_freshly_reprojected_copy_is_young_however_old_the_object_is() {
        // `stable_secret` pins creationTimestamp to 400 days ago — a copy that has
        // been refreshed in place for over a year. What matters is when a run last
        // WROTE it, which is what `projected-at` records; without it this Secret would
        // look ancient and be reaped out from under the run that just re-projected it.
        let just_written = stable_secret("media", "app-creds-0", "uid-1", 30);
        assert!(
            terminal_creds_candidates(
                &[just_written],
                &uids(&["uid-1"]),
                &HashSet::new(),
                HOUR,
                NOW
            )
            .is_empty()
        );
    }

    #[test]
    fn the_creds_min_age_floor_survives_a_zeroed_knob() {
        // `min_age_secs` is shared with the work-spec ConfigMap sweep and legally
        // accepts 0. Zeroing it there must not strip the creds kernels' guard on the
        // project→create-Job window.
        let s = stable_secret("media", "app-creds-0", "uid-1", 60);
        assert!(
            terminal_creds_candidates(&[s], &uids(&["uid-1"]), &HashSet::new(), 0, NOW).is_empty()
        );
        let old = stable_secret(
            "media",
            "app-creds-0",
            "uid-1",
            CREDS_MIN_AGE_FLOOR_SECS + 1,
        );
        assert_eq!(
            terminal_creds_candidates(&[old], &uids(&["uid-1"]), &HashSet::new(), 0, NOW).len(),
            1
        );
    }

    #[test]
    fn the_two_creds_kernels_partition_the_population() {
        // Neither kernel may ever select the other's victim: every projected copy is
        // legacy (marker-less) XOR stable (marker-bearing), and a copy that fell
        // through both would leak forever while looking swept.
        let legacy = legacy_secret("media", "app-q-1751831000-creds-0", 2 * HOUR);
        let stable = stable_secret("media", "app-20260712-creds-0", "uid-1", 2 * HOUR);
        let all = vec![legacy, stable];
        let terminal = uids(&["uid-1"]);

        let by_legacy = secret_names(&legacy_creds_candidates(&all, &HashSet::new(), HOUR, NOW));
        let by_terminal = secret_names(&terminal_creds_candidates(
            &all,
            &terminal,
            &HashSet::new(),
            HOUR,
            NOW,
        ));
        assert_eq!(by_legacy, vec!["app-q-1751831000-creds-0"]);
        assert_eq!(by_terminal, vec!["app-20260712-creds-0"]);
        assert!(by_legacy.iter().all(|n| !by_terminal.contains(n)));
    }

    #[test]
    fn the_kopia_ui_credential_mirror_is_outside_the_sweeps_selector() {
        // The sweep's in-use test reads JOBS only, so a non-Job consumer of a
        // `component=credentials` Secret would be invisible to it. The one such
        // consumer — the kopia web-UI Deployment — is safe purely because its mirrored
        // Secret carries a DIFFERENT component value, i.e. it never even enters the
        // list this sweep classifies. That is load-bearing and was untested.
        assert_ne!(
            CREDS_COMPONENT,
            crate::consts::SERVER_COMPONENT_VALUE,
            "the kopia-UI credential mirror must not share the projected-creds component \
             label, or the sweep would classify a Secret a live Deployment mounts"
        );
    }

    #[test]
    fn live_env_from_reference_shields_the_secret() {
        // A live Job that loads `app-creds-0` via envFrom shields it — the
        // exact in-use test. Covers ANY consuming Job name (backup Job `app`,
        // a populate Restore's `app-populate`, …) without name algebra.
        let s = legacy_secret("media", "app-creds-0", 2 * HOUR);
        let live = creds_refs(&[("media", "app-creds-0")]);
        assert!(legacy_creds_candidates(std::slice::from_ref(&s), &live, HOUR, NOW).is_empty());
        // A reference from ANOTHER namespace is a different run's Secret.
        let live = creds_refs(&[("automation", "app-creds-0")]);
        assert_eq!(
            secret_names(&legacy_creds_candidates(
                std::slice::from_ref(&s),
                &live,
                HOUR,
                NOW
            )),
            vec!["app-creds-0"]
        );
        // A busy sibling Job that does NOT reference the copy never shields it
        // (the old name-prefix heuristic would have deferred healing forever).
        let live = creds_refs(&[("media", "app-hourly-q-1751831000-creds-0")]);
        assert_eq!(
            secret_names(&legacy_creds_candidates(&[s], &live, HOUR, NOW)),
            vec!["app-creds-0"]
        );
    }

    #[test]
    fn job_env_from_secret_names_reads_containers_and_init_containers() {
        use k8s_openapi::api::batch::v1::JobSpec;
        use k8s_openapi::api::core::v1::{
            Container, EnvFromSource, PodSpec, PodTemplateSpec, SecretEnvSource,
        };
        let env_from = |name: &str| EnvFromSource {
            secret_ref: Some(SecretEnvSource {
                name: name.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let job = Job {
            spec: Some(JobSpec {
                template: PodTemplateSpec {
                    spec: Some(PodSpec {
                        containers: vec![Container {
                            env_from: Some(vec![env_from("app-creds-0")]),
                            ..Default::default()
                        }],
                        init_containers: Some(vec![Container {
                            env_from: Some(vec![env_from("app-creds-1")]),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            job_env_from_secret_names(&job),
            vec!["app-creds-0".to_string(), "app-creds-1".to_string()]
        );
        assert!(job_env_from_secret_names(&Job::default()).is_empty());
    }

    #[test]
    fn young_legacy_copy_is_skipped_until_min_age() {
        let s = legacy_secret("media", "app-creds-0", HOUR - 1);
        assert!(legacy_creds_candidates(&[s], &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn non_projected_secrets_are_never_selected() {
        // Unmanaged (defense in depth against an unfiltered list).
        let mut unmanaged = legacy_secret("media", "user-creds-0", 2 * HOUR);
        unmanaged
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .remove(MANAGED_BY_LABEL);
        // Managed but a different component (e.g. the kopia-UI mirror).
        let mut other_component = legacy_secret("media", "srv-kopia-ui-repo-creds-0", 2 * HOUR);
        other_component
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .insert(COMPONENT_LABEL.to_string(), "kopia-server".to_string());
        // No projected-from annotation: not a projected copy at all.
        let mut unannotated = legacy_secret("media", "app-creds-0", 2 * HOUR);
        unannotated.metadata.annotations = None;
        // Name not matching the `-creds-<digits>` shape.
        let odd_name = legacy_secret("media", "app-creds-final", 2 * HOUR);
        let secrets = vec![unmanaged, other_component, unannotated, odd_name];
        assert!(legacy_creds_candidates(&secrets, &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn unmanaged_cm_is_never_selected() {
        // Defense in depth: the server-side label selector already filters,
        // but the kernel re-checks so it is safe against an unfiltered list.
        let cms = vec![cm("media", "user-cm", 2 * HOUR, &[WORK_SPEC_FILE], false)];
        assert!(sweep_candidates(&cms, &HashSet::new(), HOUR, NOW).is_empty());
    }

    // --- batch delete-Job leak backstop (M5a) --------------------------------

    use crate::consts::{DELETE_MEMBERS_ANNOTATION, OP_LABEL, OP_SNAPSHOT_DELETE_BATCH};
    use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};

    /// A batch delete Job in `kopiur-system` created `age_secs` ago covering
    /// `members` (the annotation), in `terminal` state (`None` = live). Labelled as
    /// the dispatcher labels it unless `managed`/`op` are toggled by mutation.
    fn batch_job(name: &str, members: &[&str], age_secs: i64, terminal: Option<bool>) -> Job {
        let labels = BTreeMap::from([
            (MANAGED_BY_LABEL.to_string(), MANAGED_BY_VALUE.to_string()),
            (OP_LABEL.to_string(), OP_SNAPSHOT_DELETE_BATCH.to_string()),
        ]);
        let annotations =
            BTreeMap::from([(DELETE_MEMBERS_ANNOTATION.to_string(), members.join(","))]);
        let status = terminal.map(|ok| JobStatus {
            conditions: Some(vec![JobCondition {
                type_: if ok {
                    "Complete".into()
                } else {
                    "Failed".into()
                },
                status: "True".into(),
                ..Default::default()
            }]),
            succeeded: ok.then_some(1),
            ..Default::default()
        });
        Job {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some("kopiur-system".to_string()),
                labels: Some(labels),
                annotations: Some(annotations),
                creation_timestamp: Some(Time(
                    k8s_openapi::jiff::Timestamp::from_second(NOW - age_secs).unwrap(),
                )),
                ..Default::default()
            },
            spec: None,
            status,
        }
    }

    fn job_names(picked: &[&Job]) -> Vec<String> {
        picked.iter().map(|j| j.name_any()).collect()
    }

    #[test]
    fn old_terminal_batch_job_with_all_members_drained_is_swept() {
        let jobs = vec![batch_job("snapdel-x", &["a", "b"], 2 * HOUR, Some(true))];
        // No member holds the finalizer => every member drained => leak.
        let picked = batch_job_sweep_candidates(&jobs, &HashSet::new(), HOUR, NOW);
        assert_eq!(job_names(&picked), vec!["snapdel-x"]);
    }

    #[test]
    fn a_member_still_holding_the_finalizer_spares_the_batch_job() {
        // The delete is still pending for member `b` — never reap the Job out from
        // under it (a succeeded Job not yet observed, or a failed Job about to re-fire).
        let jobs = vec![batch_job("snapdel-x", &["a", "b"], 2 * HOUR, Some(true))];
        let holding: HashSet<String> = ["b".to_string()].into();
        assert!(batch_job_sweep_candidates(&jobs, &holding, HOUR, NOW).is_empty());
    }

    #[test]
    fn a_live_batch_job_is_never_swept() {
        // A live batch is the dispatcher's to poll, whatever the finalizer set.
        let jobs = vec![batch_job("snapdel-live", &["a"], 2 * HOUR, None)];
        assert!(batch_job_sweep_candidates(&jobs, &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn a_young_terminal_batch_job_is_skipped_until_min_age() {
        let jobs = vec![batch_job("snapdel-fresh", &["a"], HOUR - 1, Some(true))];
        assert!(batch_job_sweep_candidates(&jobs, &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn a_failed_batch_job_fully_drained_is_a_leak_too() {
        // A failed Job whose members all eventually succeeded elsewhere (drained) is
        // pure leakage — the backstop reaps it just like a succeeded one.
        let jobs = vec![batch_job("snapdel-failed", &["a"], 2 * HOUR, Some(false))];
        assert_eq!(
            job_names(&batch_job_sweep_candidates(
                &jobs,
                &HashSet::new(),
                HOUR,
                NOW
            )),
            vec!["snapdel-failed"]
        );
    }

    #[test]
    fn a_non_batch_or_unmanaged_job_is_never_swept() {
        // Missing the op label (a different kopiur Job).
        let mut not_batch = batch_job("restore-x", &["a"], 2 * HOUR, Some(true));
        not_batch.metadata.labels.as_mut().unwrap().remove(OP_LABEL);
        assert!(batch_job_sweep_candidates(&[not_batch], &HashSet::new(), HOUR, NOW).is_empty());
        // Missing managed-by (defense in depth against an unfiltered list).
        let mut unmanaged = batch_job("snapdel-y", &["a"], 2 * HOUR, Some(true));
        unmanaged
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .remove(MANAGED_BY_LABEL);
        assert!(batch_job_sweep_candidates(&[unmanaged], &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn finalizer_holding_snapshot_uids_selects_only_finalizer_bearers() {
        use kopiur_api::snapshot::SnapshotSpec;
        let mk = |name: &str, uid: &str, has_finalizer: bool| {
            let mut s = Snapshot::new(
                name,
                SnapshotSpec {
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
            s.metadata.uid = Some(uid.to_string());
            if has_finalizer {
                s.metadata.finalizers = Some(vec![SNAPSHOT_CLEANUP_FINALIZER.to_string()]);
            }
            s
        };
        let snaps = vec![mk("a", "uid-a", true), mk("b", "uid-b", false)];
        let holding = finalizer_holding_snapshot_uids(&snaps);
        assert_eq!(holding, HashSet::from(["uid-a".to_string()]));
    }
}
