//! Periodic cleanup of **central persistent cache claims** (#760): the
//! `mover.cache.mode: Persistent` PVCs of `execution.mode: Central` movers,
//! which live in the operator namespace.
//!
//! Such a claim cannot carry an owner reference (its owner is in another
//! namespace), so the garbage collector never reaches it. The owner's finalizer
//! deletes it with the owner, and [`crate::io::ensure_cache_pvc`] deletes a
//! claim whose owner can no longer do that. This pass is the backstop for what
//! neither sees, and for claims a live owner no longer uses. A tenant cannot
//! delete a PVC in the operator namespace, so nothing else would:
//!
//! - **The owner is gone**: its finalizer was removed by hand, or the operator
//!   was downgraded to a version whose finalizer does not know these claims.
//! - **A live `SnapshotPolicy` no longer uses the claim**: the policy stopped
//!   backing up to the claim's repository, the repository stopped using
//!   `execution.mode: Central`, the cache stopped being `mode: Persistent`, or
//!   the policy no longer selects the source PVC of a per-source claim.
//! - **The in-namespace predecessor**: once a policy keeps its cache centrally
//!   for a repository, the `kopiur-cache-<policy>` claim it used beside itself
//!   for that repository can never be mounted again.
//!
//! The decision ([`claim_verdict`]) is pure and conservative: anything the pass
//! cannot read with certainty is kept. A deleted cache costs one cold run and
//! no backup data, but a claim is only deleted on positive evidence, and never
//! from under a mover ([`retire_and_delete`]).

use std::collections::HashSet;

use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kube::api::ListParams;
use kube::core::DynamicObject;
use kube::{Api, ResourceExt};

use kopiur_api::common::{CacheDefaults, CacheVolumeMode, RepositoryKind};
use kopiur_api::{ClusterRepository, ExecutionMode, SnapshotPolicy};

use crate::error::{Error, Result};
use crate::io::{
    self, CACHE_PVC_COMPONENT, CACHE_PVC_COMPONENT_LABEL, CACHE_REPOSITORY_ANNOTATION,
    CACHE_SOURCE_ANNOTATION, CENTRAL_POLICY_ANNOTATION, CENTRAL_POLICY_UID_LABEL,
    CENTRAL_RESTORE_ANNOTATION, CENTRAL_RESTORE_UID_LABEL,
};

/// Never delete a claim younger than this, whatever the configured sweep
/// min-age: a run creates the claim before the cache has caught up with the
/// spec edit that made the run want it.
pub const CACHE_MIN_AGE_FLOOR_SECS: i64 = 15 * 60;

/// The kind of object a central cache claim names as its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOwnerKind {
    /// Marked with [`CENTRAL_POLICY_UID_LABEL`].
    SnapshotPolicy,
    /// Marked with [`CENTRAL_RESTORE_UID_LABEL`].
    Restore,
}

/// The owner a central cache claim names, read off its marks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimOwner {
    /// What the owner is.
    pub kind: ClaimOwnerKind,
    /// The owner's namespace.
    pub namespace: String,
    /// The owner's name.
    pub name: String,
    /// The owner's UID.
    pub uid: String,
}

/// The owner `claim` names, or `None` when it is not a central cache claim (an
/// owner-referenced cache of a `ClusterRepository` lives in the operator
/// namespace too) or its marks are incomplete. Pure.
pub fn claim_owner(claim: &PersistentVolumeClaim) -> Option<ClaimOwner> {
    let (labels, annotations) = (claim.labels(), claim.annotations());
    let (kind, uid, owner) = if let Some(uid) = labels.get(CENTRAL_POLICY_UID_LABEL) {
        (
            ClaimOwnerKind::SnapshotPolicy,
            uid,
            annotations.get(CENTRAL_POLICY_ANNOTATION)?,
        )
    } else {
        (
            ClaimOwnerKind::Restore,
            labels.get(CENTRAL_RESTORE_UID_LABEL)?,
            annotations.get(CENTRAL_RESTORE_ANNOTATION)?,
        )
    };
    let (namespace, name) = owner.split_once('/')?;
    Some(ClaimOwner {
        kind,
        namespace: namespace.to_string(),
        name: name.to_string(),
        uid: uid.clone(),
    })
}

/// What the pass found out about a claim's owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerState {
    /// No object of that namespace and name, or one with another UID.
    Gone,
    /// The owner is being deleted. Its finalizer deletes its claims.
    Terminating,
    /// A live `Restore`. Its claims stay until it is deleted.
    LiveRestore,
    /// A live `SnapshotPolicy`, and whether it still uses this claim.
    LivePolicy(PolicyUse),
}

/// Whether a live `SnapshotPolicy` still uses one of its central cache claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyUse {
    /// The policy still backs up to the claim's repository with a central
    /// persistent cache, and the claim's source PVC (if it names one) exists.
    InUse,
    /// The policy no longer names the claim's repository.
    RepositoryDropped,
    /// The claim's repository no longer runs movers centrally.
    NotCentral,
    /// The policy's effective cache for that repository is not a sized
    /// `mode: Persistent` one.
    NotPersistent,
    /// The source PVC a per-source claim serves no longer exists, or no
    /// `pvcSelector` source of the policy selects it any more.
    SourceNotSelected,
    /// Something needed for the decision could not be read (the claim names no
    /// repository, or the repository does not exist right now).
    Unknown,
}

/// What to do with a central cache claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Leave it.
    Keep,
    /// Delete it, for this reason (logged).
    Delete(&'static str),
}

/// Decide what to do with a central cache claim of age `age_secs` (`None` when
/// it carries no creation timestamp) whose owner is in `state`. Pure, and
/// exhaustive (no `_ =>`): a new state must decide here.
pub fn claim_verdict(age_secs: Option<i64>, min_age_secs: i64, state: OwnerState) -> Verdict {
    let old_enough = age_secs.is_some_and(|age| age >= min_age_secs.max(CACHE_MIN_AGE_FLOOR_SECS));
    if !old_enough {
        return Verdict::Keep;
    }
    match state {
        OwnerState::Gone => Verdict::Delete("its owner no longer exists"),
        OwnerState::Terminating | OwnerState::LiveRestore => Verdict::Keep,
        OwnerState::LivePolicy(policy_use) => match policy_use {
            PolicyUse::InUse | PolicyUse::Unknown => Verdict::Keep,
            PolicyUse::RepositoryDropped => {
                Verdict::Delete("its SnapshotPolicy no longer backs up to its repository")
            }
            PolicyUse::NotCentral => {
                Verdict::Delete("its repository no longer sets `execution.mode: Central`")
            }
            PolicyUse::NotPersistent => {
                Verdict::Delete("its SnapshotPolicy no longer has a sized `mode: Persistent` cache")
            }
            PolicyUse::SourceNotSelected => {
                Verdict::Delete("its SnapshotPolicy no longer selects the source PVC it serves")
            }
        },
    }
}

/// Whether `policy` still uses a central persistent cache for the
/// `ClusterRepository` named `repository`. `repo` is that repository as it is
/// now (`None` when it does not exist), and `source_selected` is whether the
/// claim's source PVC exists and the policy still selects it
/// ([`policy_selects`]; `None` for a claim that names no source). Pure.
pub fn policy_use(
    policy: &SnapshotPolicy,
    repository: Option<&str>,
    repo: Option<&ClusterRepository>,
    source_selected: Option<bool>,
) -> PolicyUse {
    let Some(repository) = repository else {
        return PolicyUse::Unknown;
    };
    let targets: &[kopiur_api::common::RepositoryRef] =
        match kopiur_api::policy_repositories(&policy.spec) {
            Ok(kopiur_api::PolicyRepositories::Single(r)) => std::slice::from_ref(r),
            Ok(kopiur_api::PolicyRepositories::Multi(rs)) => rs,
            Err(_) => return PolicyUse::Unknown,
        };
    if !targets
        .iter()
        .any(|r| r.kind == RepositoryKind::ClusterRepository && r.name == repository)
    {
        return PolicyUse::RepositoryDropped;
    }
    let Some(repo) = repo else {
        return PolicyUse::Unknown;
    };
    // Exhaustive (no `_ =>`): a new execution mode must decide whether its
    // movers can mount a claim in the operator namespace.
    match repo.spec.execution.map(|e| e.mode).unwrap_or_default() {
        ExecutionMode::Central => {}
        ExecutionMode::InNamespace => return PolicyUse::NotCentral,
    }
    let effective = CacheDefaults::merge(
        repo.spec
            .mover_defaults
            .as_ref()
            .and_then(|m| m.cache.as_ref()),
        policy.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
    );
    let persistent = effective.is_some_and(|c| {
        c.capacity.is_some()
            && match c.effective_mode() {
                CacheVolumeMode::Persistent => true,
                CacheVolumeMode::Ephemeral => false,
            }
    });
    if !persistent {
        return PolicyUse::NotPersistent;
    }
    match source_selected {
        Some(false) => PolicyUse::SourceNotSelected,
        Some(true) | None => PolicyUse::InUse,
    }
}

/// Whether a `pvcSelector` source of `policy` selects `source` (a PVC of the
/// policy's namespace). A per-source claim exists only for such a source, so a
/// PVC that no selector matches any more (its label was removed, or the
/// selector was narrowed) gets no further runs, and its claim no further use.
/// Pure.
pub fn policy_selects(policy: &SnapshotPolicy, source: &PersistentVolumeClaim) -> bool {
    policy.spec.sources.iter().any(|s| {
        s.pvc_selector.as_ref().is_some_and(|selector| {
            selector.label_selector.as_ref().is_none_or(|labels| {
                kopiur_api::expand::labels_match_selector(source.labels(), labels)
            })
        })
    })
}

/// The name of the claim `policy` used BESIDE ITSELF for `repository` before
/// that repository ran its movers centrally
/// (`kopiur_api::expand::cache_pvc_name`), or `None` when the policy does not
/// name that `ClusterRepository`. Pure.
pub fn in_namespace_predecessor(policy: &SnapshotPolicy, repository: &str) -> Option<String> {
    let namespace = policy.namespace()?;
    let targets: &[kopiur_api::common::RepositoryRef] =
        match kopiur_api::policy_repositories(&policy.spec).ok()? {
            kopiur_api::PolicyRepositories::Single(r) => std::slice::from_ref(r),
            kopiur_api::PolicyRepositories::Multi(rs) => rs,
        };
    let target = targets
        .iter()
        .find(|r| r.kind == RepositoryKind::ClusterRepository && r.name == repository)?;
    Some(kopiur_api::expand::cache_pvc_name(
        &policy.name_any(),
        &namespace,
        kopiur_api::is_multi_repo(&policy.spec).then_some(target),
    ))
}

/// Whether `pvc` is the in-namespace persistent cache of the `SnapshotPolicy`
/// with UID `policy_uid`: a cache claim, owner-referenced to that policy. Pure.
pub fn is_in_namespace_cache_of(pvc: &PersistentVolumeClaim, policy_uid: &str) -> bool {
    pvc.labels()
        .get(CACHE_PVC_COMPONENT_LABEL)
        .is_some_and(|v| v == CACHE_PVC_COMPONENT)
        && pvc
            .owner_references()
            .iter()
            .any(|r| r.kind == "SnapshotPolicy" && r.uid == policy_uid)
}

/// How long a retired claim is kept before it can be deleted.
///
/// A run reads its claim in [`io::ensure_cache_pvc`] and creates its Job a few
/// API calls later, in the same reconcile pass. A run that read the claim just
/// before it was retired has no Job yet. After this long it has one, and the
/// Job check in [`retire_and_delete`] sees it.
pub const RETIRE_GRACE_SECS: i64 = 60;

/// When `meta` was retired (unix seconds), read off
/// [`io::CACHE_RETIRED_ANNOTATION`]. `None` for a claim that is not retired, or
/// whose mark is not a time. Pure.
pub fn retired_at(meta: &kube::core::ObjectMeta) -> Option<i64> {
    let value = meta
        .annotations
        .as_ref()?
        .get(io::CACHE_RETIRED_ANNOTATION)?;
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|t| t.timestamp())
}

/// Whether a claim retired at `retired_at` has been kept for
/// [`RETIRE_GRACE_SECS`]. Pure.
pub fn grace_elapsed(retired_at: i64, now: i64) -> bool {
    now - retired_at >= RETIRE_GRACE_SECS
}

/// Whether one of `jobs` can still start a pod that mounts the claim named
/// `claim`: its pod template mounts the claim, and it is neither finished nor
/// being deleted. A Job with retries left starts a new pod after an attempt
/// fails, long after the `Snapshot` it serves reported that failure. Pure.
pub fn claim_has_a_job(jobs: &[k8s_openapi::api::batch::v1::Job], claim: &str) -> bool {
    jobs.iter().any(|job| {
        job.metadata.deletion_timestamp.is_none()
            && io::mover_job_terminal(job).is_none()
            && job
                .spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .and_then(|s| s.volumes.as_ref())
                .is_some_and(|volumes| {
                    volumes.iter().any(|v| {
                        v.persistent_volume_claim
                            .as_ref()
                            .is_some_and(|c| c.claim_name == claim)
                    })
                })
    })
}

/// Mark `pvc` as retired ([`io::CACHE_RETIRED_ANNOTATION`]), so that no run
/// mounts it from now on, and return when it was retired (unix seconds). An
/// existing mark is kept. The patch carries the UID of `pvc`, so it can only
/// mark the claim that was read. `None` when that claim is gone or was
/// replaced.
async fn retire_claim(
    api: &Api<PersistentVolumeClaim>,
    pvc: &PersistentVolumeClaim,
    now: i64,
) -> Result<Option<i64>> {
    if let Some(at) = retired_at(&pvc.metadata) {
        return Ok(Some(at));
    }
    let stamp = chrono::DateTime::from_timestamp(now, 0)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339();
    let patch = serde_json::json!({
        "metadata": {
            "uid": pvc.metadata.uid,
            "annotations": { io::CACHE_RETIRED_ANNOTATION: stamp },
        },
    });
    match api
        .patch(
            &pvc.name_any(),
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(&patch),
        )
        .await
    {
        Ok(_) => Ok(Some(now)),
        Err(kube::Error::Api(e)) if e.code == 404 || io::refused_for_uid(&e) => Ok(None),
        Err(e) => Err(Error::Kube(e)),
    }
}

/// What [`retire_and_delete`] did with the claims it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Retirement {
    /// Claims whose delete was accepted.
    pub deleted: usize,
    /// Claims that are retired and still kept: their grace period is not over,
    /// or a Job that mounts them is not finished. The caller comes back later.
    pub waiting: usize,
}

/// Delete persistent cache `claims` without taking one from under a mover.
///
/// Kubernetes only protects a claim from deletion while a SCHEDULED pod mounts
/// it. A pod that is not scheduled yet, the next pod of a Job with retries
/// left, and a run that picked the claim but has not created its Job yet are
/// all unprotected: delete the claim, and that pod waits for a PVC that does
/// not exist until its startup deadline fails the run. So a claim goes in
/// three steps, each of which a later call continues:
///
/// 1. **Retire it.** From then on no run mounts it
///    ([`io::CachePvcClaim::Terminating`]).
/// 2. **Keep it for [`RETIRE_GRACE_SECS`].** A run that read the claim before
///    step 1 creates its Job in that time.
/// 3. **Delete it once no unfinished Job mounts it** ([`claim_has_a_job`]).
///
/// The test is about the claim, not about the `Snapshot`s of its policy: a
/// `Snapshot` can reference a policy of another namespace without the policy
/// label, it reports `Failed` while its Job still has retries, and the runs
/// that start after step 1 do not mount the claim at all.
///
/// `now` is the decision clock (unix seconds).
pub async fn retire_and_delete(
    client: &kube::Client,
    claims: &[(Api<PersistentVolumeClaim>, PersistentVolumeClaim)],
    now: i64,
) -> Result<Retirement> {
    use k8s_openapi::api::batch::v1::Job;
    let managed = ListParams::default().labels(&format!(
        "{}={}",
        crate::consts::MANAGED_BY_LABEL,
        crate::consts::MANAGED_BY_VALUE
    ));
    // The Jobs of each namespace, listed at most once, and only after a claim
    // of that namespace finished its grace period.
    let mut jobs: std::collections::HashMap<String, Vec<Job>> = std::collections::HashMap::new();
    let mut outcome = Retirement::default();
    for (api, pvc) in claims {
        let Some(retired) = retire_claim(api, pvc, now).await? else {
            continue;
        };
        if !grace_elapsed(retired, now) {
            outcome.waiting += 1;
            continue;
        }
        let namespace = pvc.namespace().unwrap_or_default();
        if !jobs.contains_key(&namespace) {
            let listed = Api::<Job>::namespaced(client.clone(), &namespace)
                .list(&managed)
                .await?
                .items;
            jobs.insert(namespace.clone(), listed);
        }
        if claim_has_a_job(&jobs[&namespace], &pvc.name_any()) {
            outcome.waiting += 1;
        } else if delete_claim(api, pvc).await? {
            outcome.deleted += 1;
        }
    }
    Ok(outcome)
}

/// What one pass deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheSweepOutcome {
    /// Central claims deleted.
    pub central: usize,
    /// In-namespace predecessors deleted.
    pub predecessors: usize,
    /// Retired claims outside the operator namespace deleted (the second step
    /// of a predecessor, or of a cache reset whose policy stopped reconciling).
    pub retired: usize,
}

/// Delete `pvc` exactly (its UID is a precondition). `true` when the delete was
/// accepted; a claim that is gone or was replaced is not an error.
async fn delete_claim(
    api: &Api<PersistentVolumeClaim>,
    pvc: &PersistentVolumeClaim,
) -> Result<bool> {
    match api
        .delete(&pvc.name_any(), &io::delete_exactly(&pvc.metadata))
        .await
    {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => Ok(false),
        Err(e) => Err(Error::Kube(e)),
    }
}

/// Read the state of `owner` for the claim `claim`, from the API server.
async fn owner_state(
    client: &kube::Client,
    claim: &PersistentVolumeClaim,
    owner: &ClaimOwner,
) -> Result<(OwnerState, Option<SnapshotPolicy>)> {
    let live = |meta: &kube::core::ObjectMeta| -> Option<OwnerState> {
        if meta.uid.as_deref() != Some(owner.uid.as_str()) {
            return Some(OwnerState::Gone);
        }
        meta.deletion_timestamp
            .is_some()
            .then_some(OwnerState::Terminating)
    };
    match owner.kind {
        ClaimOwnerKind::Restore => {
            let restores: Api<DynamicObject> = Api::namespaced_with(
                client.clone(),
                &owner.namespace,
                &io::CentralCacheOwnerKind::Restore.api_resource(),
            );
            Ok(match restores.get_opt(&owner.name).await? {
                None => (OwnerState::Gone, None),
                Some(restore) => (
                    live(&restore.metadata).unwrap_or(OwnerState::LiveRestore),
                    None,
                ),
            })
        }
        ClaimOwnerKind::SnapshotPolicy => {
            let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), &owner.namespace);
            let Some(policy) = policies.get_opt(&owner.name).await? else {
                return Ok((OwnerState::Gone, None));
            };
            if let Some(state) = live(&policy.metadata) {
                return Ok((state, None));
            }
            let annotations = claim.annotations();
            let repository = annotations.get(CACHE_REPOSITORY_ANNOTATION);
            let repo = match repository {
                Some(name) => {
                    let repos: Api<ClusterRepository> = Api::all(client.clone());
                    repos.get_opt(name).await?
                }
                None => None,
            };
            let source_selected = match annotations.get(CACHE_SOURCE_ANNOTATION) {
                Some(source) => {
                    let pvcs: Api<PersistentVolumeClaim> =
                        Api::namespaced(client.clone(), &owner.namespace);
                    Some(
                        pvcs.get_opt(source)
                            .await?
                            .is_some_and(|pvc| policy_selects(&policy, &pvc)),
                    )
                }
                None => None,
            };
            let state = OwnerState::LivePolicy(policy_use(
                &policy,
                repository.map(String::as_str),
                repo.as_ref(),
                source_selected,
            ));
            Ok((state, Some(policy)))
        }
    }
}

/// One pass over the central cache claims in `central_ns`. A claim that
/// cannot be judged or deleted is logged and skipped (degrade, do not crash).
pub async fn run_cache_sweep(
    client: &kube::Client,
    central_ns: &str,
    min_age_secs: i64,
) -> Result<CacheSweepOutcome> {
    let central: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), central_ns);
    let caches = ListParams::default().labels(&format!(
        "{CACHE_PVC_COMPONENT_LABEL}={CACHE_PVC_COMPONENT}"
    ));
    let pass = Pass {
        client,
        central: &central,
        min_age_secs,
        now: chrono::Utc::now().timestamp(),
    };
    let mut outcome = CacheSweepOutcome::default();
    let mut predecessors_seen: HashSet<(String, String)> = HashSet::new();
    for claim in central.list(&caches).await?.items {
        let Some(owner) = claim_owner(&claim) else {
            continue;
        };
        if claim.metadata.deletion_timestamp.is_some() {
            continue;
        }
        match pass
            .sweep_claim(&claim, &owner, &mut predecessors_seen)
            .await
        {
            Ok(swept) => {
                outcome.central += swept.central;
                outcome.predecessors += swept.predecessors;
            }
            Err(e) => {
                tracing::warn!(claim = %claim.name_any(), namespace = %central_ns, error = %e,
                    "could not sweep a central cache PVC (skipped)");
            }
        }
    }
    outcome.retired = pass.drain_retired(central_ns).await?;
    Ok(outcome)
}

/// What every claim of one pass is judged against.
struct Pass<'a> {
    client: &'a kube::Client,
    /// The claims of the operator namespace.
    central: &'a Api<PersistentVolumeClaim>,
    min_age_secs: i64,
    /// The decision clock (unix seconds).
    now: i64,
}

impl Pass<'_> {
    /// Finish the deletion of every retired cache claim outside the operator
    /// namespace: a predecessor that an earlier pass retired, or the claims of
    /// a cache reset. A retired claim is never mounted again, so one that is
    /// left behind keeps every later run of its policy on a per-run volume.
    /// This does not depend on what retired it still being true (the
    /// repository can have left `Central` since). Returns the deletes accepted.
    async fn drain_retired(&self, central_ns: &str) -> Result<usize> {
        let all: Api<PersistentVolumeClaim> = Api::all(self.client.clone());
        let caches = ListParams::default().labels(&format!(
            "{CACHE_PVC_COMPONENT_LABEL}={CACHE_PVC_COMPONENT}"
        ));
        let retired: Vec<_> = all
            .list(&caches)
            .await?
            .items
            .into_iter()
            .filter(|pvc| {
                io::is_retired(&pvc.metadata)
                    && pvc.metadata.deletion_timestamp.is_none()
                    && pvc.namespace().as_deref() != Some(central_ns)
            })
            .map(|pvc| {
                let api =
                    Api::namespaced(self.client.clone(), &pvc.namespace().unwrap_or_default());
                (api, pvc)
            })
            .collect();
        Ok(retire_and_delete(self.client, &retired, self.now)
            .await?
            .deleted)
    }

    /// Judge one central claim, and delete it or the predecessor it replaced.
    async fn sweep_claim(
        &self,
        claim: &PersistentVolumeClaim,
        owner: &ClaimOwner,
        predecessors_seen: &mut HashSet<(String, String)>,
    ) -> Result<CacheSweepOutcome> {
        let (state, policy) = owner_state(self.client, claim, owner).await?;
        let age = claim
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| self.now - t.0.as_second());
        let mut swept = CacheSweepOutcome::default();
        // A retired claim was judged by an earlier pass or a reset, which is
        // waiting for its grace period or for a Job. No new run mounts it, so
        // it only goes.
        let verdict = if io::is_retired(&claim.metadata) {
            Verdict::Delete("it was retired")
        } else {
            claim_verdict(age, self.min_age_secs, state)
        };
        if let Verdict::Delete(reason) = verdict
            && self.delete_central(claim, state).await?
        {
            swept.central = 1;
            tracing::info!(claim = %claim.name_any(),
                owner = %format!("{}/{}", owner.namespace, owner.name), reason,
                "deleted a central cache PVC");
        }
        // A policy that keeps its cache centrally for a repository can never
        // mount the claim it used beside itself for that repository again.
        if let (OwnerState::LivePolicy(PolicyUse::InUse), Some(policy)) = (state, policy)
            && self
                .delete_predecessor(claim, owner, &policy, predecessors_seen)
                .await?
        {
            swept.predecessors = 1;
        }
        Ok(swept)
    }

    /// Delete a central claim that got a `Delete` verdict. A claim of a live
    /// policy can still have a mover, so it goes through [`retire_and_delete`]
    /// and is deleted on a later pass. A claim whose owner is gone has no
    /// runs left: the finalizer of a `Snapshot` deletes its central Job.
    /// `true` when a delete was accepted.
    async fn delete_central(
        &self,
        claim: &PersistentVolumeClaim,
        state: OwnerState,
    ) -> Result<bool> {
        match state {
            OwnerState::LivePolicy(_) | OwnerState::Terminating | OwnerState::LiveRestore => {
                let claims = [(self.central.clone(), claim.clone())];
                Ok(retire_and_delete(self.client, &claims, self.now)
                    .await?
                    .deleted
                    > 0)
            }
            OwnerState::Gone => delete_claim(self.central, claim).await,
        }
    }

    /// Delete the claim `policy` used beside itself for the repository of
    /// `claim`, if it is still there. `true` when a delete was accepted.
    async fn delete_predecessor(
        &self,
        claim: &PersistentVolumeClaim,
        owner: &ClaimOwner,
        policy: &SnapshotPolicy,
        predecessors_seen: &mut HashSet<(String, String)>,
    ) -> Result<bool> {
        let Some(predecessor) = claim
            .annotations()
            .get(CACHE_REPOSITORY_ANNOTATION)
            .and_then(|repository| in_namespace_predecessor(policy, repository))
        else {
            return Ok(false);
        };
        // The claims of a many-source policy all name the same predecessor.
        if !predecessors_seen.insert((owner.namespace.clone(), predecessor.clone())) {
            return Ok(false);
        }
        let beside: Api<PersistentVolumeClaim> =
            Api::namespaced(self.client.clone(), &owner.namespace);
        // A run that began beside the policy before the repository went
        // central may still mount it, hence retire-then-delete. This pass
        // retires it, and [`Pass::drain_retired`] deletes it on a later one.
        let deleted = match beside.get_opt(&predecessor).await? {
            Some(pvc)
                if pvc.metadata.deletion_timestamp.is_none()
                    && is_in_namespace_cache_of(&pvc, &owner.uid) =>
            {
                retire_and_delete(self.client, &[(beside.clone(), pvc)], self.now)
                    .await?
                    .deleted
                    > 0
            }
            Some(_) | None => false,
        };
        if deleted {
            tracing::info!(claim = %predecessor, namespace = %owner.namespace,
                "deleted a cache PVC that central execution replaced");
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests;
