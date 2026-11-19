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
//! while a run of its policy is in flight ([`policy_run_in_flight`]).

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

/// Whether `snapshot` is a run that may still mount a cache claim: not being
/// deleted, and not in a terminal phase. A run with no phase yet counts. Pure.
pub fn run_in_flight(snapshot: &kopiur_api::Snapshot) -> bool {
    snapshot.metadata.deletion_timestamp.is_none()
        && !snapshot
            .status
            .as_ref()
            .and_then(|s| s.phase.as_ref())
            .is_some_and(kopiur_api::SnapshotPhase::is_terminal)
}

/// Whether a run of the `SnapshotPolicy` `namespace/policy` is in flight
/// ([`run_in_flight`]).
///
/// A claim must not be deleted under such a run. A run picks its claim, then
/// creates its Job, and the Job's pod is only protected by
/// `kubernetes.io/pvc-protection` once it is scheduled. A claim deleted in
/// between leaves that pod waiting on a PVC that does not exist, and the run
/// fails at its pod startup deadline.
pub async fn policy_run_in_flight(
    client: &kube::Client,
    namespace: &str,
    policy: &str,
) -> Result<bool> {
    let snapshots: Api<kopiur_api::Snapshot> = Api::namespaced(client.clone(), namespace);
    let by_policy =
        ListParams::default().labels(&format!("{}={policy}", crate::consts::CONFIG_LABEL));
    Ok(snapshots
        .list(&by_policy)
        .await?
        .items
        .iter()
        .any(run_in_flight))
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

/// What one pass deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheSweepOutcome {
    /// Central claims deleted.
    pub central: usize,
    /// In-namespace predecessors deleted.
    pub predecessors: usize,
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
        if let Verdict::Delete(reason) = claim_verdict(age, self.min_age_secs, state)
            && !self.held_by_a_run(state, owner).await?
            && delete_claim(self.central, claim).await?
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

    /// Whether a run of the claim's live policy is in flight, so the claim
    /// stays for this pass ([`policy_run_in_flight`]). A claim whose owner is
    /// gone has no runs left to wait for.
    async fn held_by_a_run(&self, state: OwnerState, owner: &ClaimOwner) -> Result<bool> {
        match state {
            OwnerState::LivePolicy(_) => {
                policy_run_in_flight(self.client, &owner.namespace, &owner.name).await
            }
            OwnerState::Gone | OwnerState::Terminating | OwnerState::LiveRestore => Ok(false),
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
        // A run that began beside the policy before the repository went
        // central may still be about to mount it.
        if policy_run_in_flight(self.client, &owner.namespace, &owner.name).await? {
            return Ok(false);
        }
        let beside: Api<PersistentVolumeClaim> =
            Api::namespaced(self.client.clone(), &owner.namespace);
        let deleted = match beside.get_opt(&predecessor).await? {
            Some(pvc)
                if pvc.metadata.deletion_timestamp.is_none()
                    && is_in_namespace_cache_of(&pvc, &owner.uid) =>
            {
                delete_claim(&beside, &pvc).await?
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
