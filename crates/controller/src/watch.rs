//! Cross-resource watch mappers: turn a change to a *referenced* object into the
//! set of *referrer* CRs that must re-reconcile.
//!
//! The kube `Controller` only re-reconciles its primary kind and its owned
//! children. Every reconciler that *reads* an external referent it does not own —
//! a credential `Secret`, a TLS-CA `ConfigMap`, a `Repository`/`ClusterRepository`,
//! a `SnapshotPolicy` — would otherwise not react when that referent changes
//! (a `Secret` content edit does not even bump the referrer's `generation`). These
//! mappers, wired via `Controller::watches` in [`crate::run`], close that gap so a
//! fixed credential or a repository that just became `Ready` re-triggers its
//! consumers within seconds instead of waiting out a multi-minute requeue.
//!
//! Each mapper reads a reflector [`Store`] of the *referrer* kind (the controller's
//! own store, shared via `Controller::store()`) and scans it in memory — never an
//! `Api::list` per event (the "prefer a shared informer over a list" rule). A
//! non-matching event (most cluster `Secret`s) yields an empty set and triggers
//! nothing.

use k8s_openapi::api::core::v1::{
    ConfigMap, Namespace, PersistentVolumeClaim, Secret, ServiceAccount,
};
use kube::ResourceExt;
use kube::core::PartialObjectMeta;
use kube::runtime::reflector::{ObjectRef, Store};

use kopiur_api::backend::Backend;
use kopiur_api::common::{Encryption, PolicyRef, RepositoryKind, RepositoryRef};
use kopiur_api::{
    ClusterRepository, Maintenance, Repository, RepositoryReplication, Restore, Snapshot,
    SnapshotPolicy, SnapshotSchedule,
};

use crate::io;
use crate::snapshot_schedule::policy_matches_selector;

// --- predicates -------------------------------------------------------------

/// Does a repository with `backend`/`encryption` (whose own namespace is
/// `repo_ns`, `None` for cluster-scoped) reference the Secret `(sec_ns, sec_name)`?
/// Covers the encryption-password Secret and the backend `auth`/rclone-config
/// Secret, with the same namespace-defaulting the mover uses
/// ([`io::mover_creds_secret_refs`]).
fn repo_references_secret(
    backend: &Backend,
    encryption: &Encryption,
    repo_ns: Option<&str>,
    sec_ns: &str,
    sec_name: &str,
) -> bool {
    io::mover_creds_secret_refs(backend, encryption, repo_ns)
        .iter()
        .any(|r| r.name == sec_name && r.namespace.as_deref() == Some(sec_ns))
}

/// Whether a consumer's [`RepositoryRef`] targets the namespaced `Repository`
/// `(repo_ns, repo_name)`. `consumer_ns` is the referrer's own namespace, used
/// when the ref omits one. A `ClusterRepository` ref never matches a `Repository`.
fn ref_targets_repository(
    r: &RepositoryRef,
    consumer_ns: Option<&str>,
    repo_ns: &str,
    repo_name: &str,
) -> bool {
    matches!(r.kind, RepositoryKind::Repository)
        && r.name == repo_name
        && r.namespace.as_deref().or(consumer_ns) == Some(repo_ns)
}

/// Whether a consumer's [`RepositoryRef`] targets the `ClusterRepository` `name`
/// (cluster-scoped: namespace is meaningless and ignored).
fn ref_targets_cluster(r: &RepositoryRef, name: &str) -> bool {
    matches!(r.kind, RepositoryKind::ClusterRepository) && r.name == name
}

/// Whether a consumer's [`PolicyRef`] (in `consumer_ns`) targets the namespaced
/// `SnapshotPolicy` `(policy_ns, policy_name)`.
fn ref_targets_policy(
    r: &PolicyRef,
    consumer_ns: Option<&str>,
    policy_ns: &str,
    policy_name: &str,
) -> bool {
    r.name == policy_name && r.namespace.as_deref().or(consumer_ns) == Some(policy_ns)
}

// --- generic store scans ----------------------------------------------------

/// Scan a referrer `Store` and return an [`ObjectRef`] for each referrer for which
/// `matches` holds. The single in-memory pass shared by every mapper below.
fn select<K, F>(store: &Store<K>, matches: F) -> Vec<ObjectRef<K>>
where
    K: kube::Resource<DynamicType = ()> + Clone + 'static,
    F: Fn(&K) -> bool,
{
    store
        .state()
        .iter()
        .filter(|obj| matches(obj.as_ref()))
        .map(|obj| ObjectRef::from_obj(obj.as_ref()))
        .collect()
}

// --- M2: Secret / ConfigMap -> repository -----------------------------------

/// Repositories that reference the changed `secret` (password or backend auth).
pub fn secret_to_repositories(
    store: &Store<Repository>,
    secret: &PartialObjectMeta<Secret>,
) -> Vec<ObjectRef<Repository>> {
    let (Some(sec_ns), sec_name) = (secret.namespace(), secret.name_any()) else {
        return Vec::new();
    };
    select(store, |r: &Repository| {
        repo_references_secret(
            &r.spec.backend,
            &r.spec.encryption,
            r.namespace().as_deref(),
            &sec_ns,
            &sec_name,
        )
    })
}

/// ClusterRepositories that reference the changed `secret`.
pub fn secret_to_cluster_repositories(
    store: &Store<ClusterRepository>,
    secret: &PartialObjectMeta<Secret>,
) -> Vec<ObjectRef<ClusterRepository>> {
    let (Some(sec_ns), sec_name) = (secret.namespace(), secret.name_any()) else {
        return Vec::new();
    };
    select(store, |r: &ClusterRepository| {
        // Cluster-scoped: refs pin their own namespace (repo_ns = None).
        repo_references_secret(
            &r.spec.backend,
            &r.spec.encryption,
            None,
            &sec_ns,
            &sec_name,
        )
    })
}

/// Repositories whose backend's `auth.workloadIdentity` names the changed
/// ServiceAccount (matched in the repository's own namespace, where its
/// bootstrap mover runs). Creating the SA un-sticks a repository blocked on the
/// `MissingDependency` preflight immediately, instead of waiting out the
/// requeue backoff (the reconcile-on-referent-change rule).
pub fn serviceaccount_to_repositories(
    store: &Store<Repository>,
    sa: &PartialObjectMeta<ServiceAccount>,
) -> Vec<ObjectRef<Repository>> {
    let (Some(sa_ns), sa_name) = (sa.namespace(), sa.name_any()) else {
        return Vec::new();
    };
    select(store, |r: &Repository| {
        kopiur_api::creds::backend_workload_identity(&r.spec.backend).is_some_and(|(wi, _)| {
            wi.service_account_name == sa_name && r.namespace().as_deref() == Some(sa_ns.as_str())
        })
    })
}

/// ClusterRepositories whose backend's `auth.workloadIdentity` names the changed
/// ServiceAccount. Matched by **name only**: a ClusterRepository's movers run in
/// many namespaces (bootstrap in the placement namespace, consumers in theirs),
/// so any namespace's SA of that name may be the one a blocked reconcile waits
/// on — a rare spurious re-reconcile is cheap and idempotent.
pub fn serviceaccount_to_cluster_repositories(
    store: &Store<ClusterRepository>,
    sa: &PartialObjectMeta<ServiceAccount>,
) -> Vec<ObjectRef<ClusterRepository>> {
    let sa_name = sa.name_any();
    select(store, |r: &ClusterRepository| {
        kopiur_api::creds::backend_workload_identity(&r.spec.backend)
            .is_some_and(|(wi, _)| wi.service_account_name == sa_name)
    })
}

/// RepositoryReplications that reference the changed `secret` as their *destination*
/// backend's auth credential. (`sync-to` is a blob copy — the destination inherits
/// the source repository's password, so there is no separate destination password to
/// watch.) A change to the *source* repository's Secret instead re-reconciles the
/// source `Repository`/`ClusterRepository`, which re-triggers this replication via
/// the source-ref watch ([`repository_to_replications`]).
pub fn secret_to_replications(
    store: &Store<RepositoryReplication>,
    secret: &PartialObjectMeta<Secret>,
) -> Vec<ObjectRef<RepositoryReplication>> {
    let (Some(sec_ns), sec_name) = (secret.namespace(), secret.name_any()) else {
        return Vec::new();
    };
    select(store, |repl: &RepositoryReplication| {
        let repl_ns = repl.namespace();
        io::backend_auth_secret_ref(&repl.spec.destination).is_some_and(|a| {
            a.name == sec_name
                && a.namespace.as_deref().or(repl_ns.as_deref()) == Some(sec_ns.as_str())
        })
    })
}

/// Repositories that reference the changed TLS-CA `configmap`. A `ConfigMapKeyRef`
/// carries no namespace, so for a namespaced `Repository` we require the ConfigMap
/// to live in the repo's namespace.
pub fn configmap_to_repositories(
    store: &Store<Repository>,
    configmap: &PartialObjectMeta<ConfigMap>,
) -> Vec<ObjectRef<Repository>> {
    let (Some(cm_ns), cm_name) = (configmap.namespace(), configmap.name_any()) else {
        return Vec::new();
    };
    select(store, |r: &Repository| {
        io::backend_tls_ca_configmap(&r.spec.backend) == Some(cm_name.as_str())
            && r.namespace().as_deref() == Some(cm_ns.as_str())
    })
}

/// ClusterRepositories that reference the changed TLS-CA `configmap`. A cluster
/// repo's `caBundleRef` has no namespace, so this matches by ConfigMap *name* only
/// (an over-trigger at worst re-runs an idempotent reconcile).
pub fn configmap_to_cluster_repositories(
    store: &Store<ClusterRepository>,
    configmap: &PartialObjectMeta<ConfigMap>,
) -> Vec<ObjectRef<ClusterRepository>> {
    let cm_name = configmap.name_any();
    select(store, |r: &ClusterRepository| {
        io::backend_tls_ca_configmap(&r.spec.backend) == Some(cm_name.as_str())
    })
}

// --- M3: repository -> consumers --------------------------------------------

/// SnapshotPolicies whose `spec.repository` targets the changed `Repository`.
pub fn repository_to_policies(
    store: &Store<SnapshotPolicy>,
    repo: &Repository,
) -> Vec<ObjectRef<SnapshotPolicy>> {
    let (Some(ns), name) = (repo.namespace(), repo.name_any()) else {
        return Vec::new();
    };
    select(store, |c: &SnapshotPolicy| {
        ref_targets_repository(&c.spec.repository, c.namespace().as_deref(), &ns, &name)
    })
}

/// SnapshotPolicies whose `spec.repository` targets the changed `ClusterRepository`.
pub fn cluster_repository_to_policies(
    store: &Store<SnapshotPolicy>,
    repo: &ClusterRepository,
) -> Vec<ObjectRef<SnapshotPolicy>> {
    let name = repo.name_any();
    select(store, |c: &SnapshotPolicy| {
        ref_targets_cluster(&c.spec.repository, &name)
    })
}

/// Restores whose `spec.repository` targets the changed `Repository`.
pub fn repository_to_restores(
    store: &Store<Restore>,
    repo: &Repository,
) -> Vec<ObjectRef<Restore>> {
    let (Some(ns), name) = (repo.namespace(), repo.name_any()) else {
        return Vec::new();
    };
    select(store, |c: &Restore| {
        c.spec
            .repository
            .as_ref()
            .is_some_and(|r| ref_targets_repository(r, c.namespace().as_deref(), &ns, &name))
    })
}

/// Restores whose `spec.repository` targets the changed `ClusterRepository`.
pub fn cluster_repository_to_restores(
    store: &Store<Restore>,
    repo: &ClusterRepository,
) -> Vec<ObjectRef<Restore>> {
    let name = repo.name_any();
    select(store, |c: &Restore| {
        c.spec
            .repository
            .as_ref()
            .is_some_and(|r| ref_targets_cluster(r, &name))
    })
}

/// Populator `Restore`s claimed by the changed `PersistentVolumeClaim` via
/// `spec.dataSourceRef`. The claim and the Restore it names share a namespace, so a
/// change to the claim re-enqueues the Restore to advance the handshake (ADR-0005 §9).
pub fn pvc_to_restores(
    store: &Store<Restore>,
    pvc: &PersistentVolumeClaim,
) -> Vec<ObjectRef<Restore>> {
    let (Some(pvc_ns), Some(dsr)) = (
        pvc.namespace(),
        pvc.spec.as_ref().and_then(|s| s.data_source_ref.as_ref()),
    ) else {
        return Vec::new();
    };
    if dsr.kind != "Restore" || dsr.api_group.as_deref() != Some("kopiur.home-operations.com") {
        return Vec::new();
    }
    let target = dsr.name.clone();
    select(store, |r: &Restore| {
        r.namespace().as_deref() == Some(pvc_ns.as_str()) && r.name_any() == target
    })
}

/// Maintenances whose `spec.repository` targets the changed `Repository`.
pub fn repository_to_maintenances(
    store: &Store<Maintenance>,
    repo: &Repository,
) -> Vec<ObjectRef<Maintenance>> {
    let (Some(ns), name) = (repo.namespace(), repo.name_any()) else {
        return Vec::new();
    };
    select(store, |c: &Maintenance| {
        ref_targets_repository(&c.spec.repository, c.namespace().as_deref(), &ns, &name)
    })
}

/// Maintenances whose `spec.repository` targets the changed `ClusterRepository`.
pub fn cluster_repository_to_maintenances(
    store: &Store<Maintenance>,
    repo: &ClusterRepository,
) -> Vec<ObjectRef<Maintenance>> {
    let name = repo.name_any();
    select(store, |c: &Maintenance| {
        ref_targets_cluster(&c.spec.repository, &name)
    })
}

/// RepositoryReplications whose `spec.sourceRef` targets the changed `Repository`.
pub fn repository_to_replications(
    store: &Store<RepositoryReplication>,
    repo: &Repository,
) -> Vec<ObjectRef<RepositoryReplication>> {
    let (Some(ns), name) = (repo.namespace(), repo.name_any()) else {
        return Vec::new();
    };
    select(store, |c: &RepositoryReplication| {
        ref_targets_repository(&c.spec.source_ref, c.namespace().as_deref(), &ns, &name)
    })
}

/// RepositoryReplications whose `spec.sourceRef` targets the changed `ClusterRepository`.
pub fn cluster_repository_to_replications(
    store: &Store<RepositoryReplication>,
    repo: &ClusterRepository,
) -> Vec<ObjectRef<RepositoryReplication>> {
    let name = repo.name_any();
    select(store, |c: &RepositoryReplication| {
        ref_targets_cluster(&c.spec.source_ref, &name)
    })
}

/// SnapshotSchedules potentially affected by a change to the namespaced
/// `Repository` — for the `scheduleDefaults.timezone` a schedule inherits when it
/// sets no `spec.schedule.timezone` (GitHub #174 item 3).
///
/// **Namespace-broadcast (deliberate simplification).** A schedule inherits from
/// its *target policy's* repository, not from a repository it references directly,
/// so the precise set is `repo → policies referencing it → schedules targeting
/// those policies`. That two-hop join needs a `SnapshotPolicy` reflector cache in
/// the schedule controller, which it does not have (and the brief says not to add a
/// full cache without need). Since a schedule's selector policies and its
/// same-namespace `policyRef` policies (and thus their repositories) live in the
/// schedule's own namespace, we enqueue every schedule in the changed repository's
/// namespace. Schedules are few and slot computation is idempotent, so a spurious
/// reconcile is cheap; the only miss is a schedule whose `policyRef` crosses into
/// another namespace to a policy backed by this repo — that rare case is still
/// covered by the schedule's own periodic requeue and by pin invalidation on the
/// next natural pin.
pub fn repository_to_schedules(
    store: &Store<SnapshotSchedule>,
    repo: &Repository,
) -> Vec<ObjectRef<SnapshotSchedule>> {
    let Some(ns) = repo.namespace() else {
        return Vec::new();
    };
    select(store, |s: &SnapshotSchedule| {
        s.namespace().as_deref() == Some(ns.as_str())
    })
}

/// SnapshotSchedules potentially affected by a change to the cluster-scoped
/// `ClusterRepository`'s `scheduleDefaults.timezone`. A `ClusterRepository` can be
/// referenced from any namespace, so — unlike the namespaced case above — the
/// affected namespaces are unbounded; enqueue every schedule cluster-wide. Same
/// cost/idempotence rationale: schedules are few, slot logic is idempotent, and
/// the effective-timezone resolution short-circuits for any schedule that sets its
/// own `spec.schedule.timezone`.
pub fn cluster_repository_to_schedules(
    store: &Store<SnapshotSchedule>,
    _repo: &ClusterRepository,
) -> Vec<ObjectRef<SnapshotSchedule>> {
    select(store, |_s: &SnapshotSchedule| true)
}

// --- M3: SnapshotPolicy -> Snapshot / SnapshotSchedule ----------------------

/// Snapshots whose `spec.policyRef` targets the changed `SnapshotPolicy`.
pub fn policy_to_snapshots(
    store: &Store<Snapshot>,
    policy: &SnapshotPolicy,
) -> Vec<ObjectRef<Snapshot>> {
    let (Some(ns), name) = (policy.namespace(), policy.name_any()) else {
        return Vec::new();
    };
    select(store, |s: &Snapshot| {
        s.spec
            .policy_ref
            .as_ref()
            .is_some_and(|r| ref_targets_policy(r, s.namespace().as_deref(), &ns, &name))
    })
}

/// SnapshotSchedules that select the changed `SnapshotPolicy` — by `policyRef`
/// (single) or by `policySelector` (fan-out; same-namespace label match, reusing
/// [`policy_matches_selector`]).
pub fn policy_to_schedules(
    store: &Store<SnapshotSchedule>,
    policy: &SnapshotPolicy,
) -> Vec<ObjectRef<SnapshotSchedule>> {
    let (Some(ns), name) = (policy.namespace(), policy.name_any()) else {
        return Vec::new();
    };
    let labels = policy.labels().clone();
    select(store, |sched: &SnapshotSchedule| {
        let sched_ns = sched.namespace();
        if let Some(r) = sched.spec.policy_ref.as_ref()
            && ref_targets_policy(r, sched_ns.as_deref(), &ns, &name)
        {
            return true;
        }
        // A selector only matches policies in the schedule's own namespace.
        sched.spec.policy_selector.as_ref().is_some_and(|sel| {
            sched_ns.as_deref() == Some(ns.as_str()) && policy_matches_selector(&labels, sel)
        })
    })
}

// --- M4: repository -> DELETING Snapshots (mass-deletion ack drain) ----------

/// Whether a `Snapshot` is terminating with our cleanup finalizer still present —
/// the set the mass-deletion ack must re-evaluate when a repository's
/// `allow-mass-deletion` annotation (or `deletionProtection`) changes.
fn snapshot_awaiting_deletion(s: &Snapshot) -> bool {
    s.metadata.deletion_timestamp.is_some()
        && s.finalizers()
            .iter()
            .any(|f| f == crate::consts::SNAPSHOT_CLEANUP_FINALIZER)
}

/// Whether a deleting `Snapshot`'s `status.resolved.repository` pin targets the
/// namespaced `Repository` `(repo_ns, repo_name)`. An UNPINNED Snapshot (pre-#255)
/// matches EVERY repository — included conservatively: re-reconciling it is cheap
/// and its finalizer simply re-evaluates (a no-op if it doesn't resolve here).
fn deleting_snapshot_targets_repository(s: &Snapshot, repo_ns: &str, repo_name: &str) -> bool {
    match s
        .status
        .as_ref()
        .and_then(|st| st.resolved.as_ref())
        .and_then(|r| r.repository.as_ref())
    {
        Some(pin) => {
            matches!(pin.kind, RepositoryKind::Repository)
                && pin.name == repo_name
                && pin.namespace.as_deref() == Some(repo_ns)
        }
        None => true,
    }
}

/// As [`deleting_snapshot_targets_repository`] but for a `ClusterRepository` pin
/// (matched by name; namespace is meaningless). Unpinned ⇒ included.
fn deleting_snapshot_targets_cluster(s: &Snapshot, name: &str) -> bool {
    match s
        .status
        .as_ref()
        .and_then(|st| st.resolved.as_ref())
        .and_then(|r| r.repository.as_ref())
    {
        Some(pin) => matches!(pin.kind, RepositoryKind::ClusterRepository) && pin.name == name,
        None => true,
    }
}

/// DELETING Snapshots resolving to the changed namespaced `Repository`, so an
/// `allow-mass-deletion` ack (or `deletionProtection` edit) re-reconciles the
/// held Snapshots at once to drain them, instead of waiting out their long `Held`
/// requeue. Matched by the `status.resolved.repository` pin; unpinned deleting
/// Snapshots are included (conservative — the finalizer no-ops if it doesn't
/// actually target this repo).
pub fn repository_to_deleting_snapshots(
    store: &Store<Snapshot>,
    repo: &Repository,
) -> Vec<ObjectRef<Snapshot>> {
    let (Some(ns), name) = (repo.namespace(), repo.name_any()) else {
        return Vec::new();
    };
    select(store, |s: &Snapshot| {
        snapshot_awaiting_deletion(s) && deleting_snapshot_targets_repository(s, &ns, &name)
    })
}

/// DELETING Snapshots resolving to the changed `ClusterRepository` (same ack-drain
/// contract as [`repository_to_deleting_snapshots`]).
pub fn cluster_repository_to_deleting_snapshots(
    store: &Store<Snapshot>,
    repo: &ClusterRepository,
) -> Vec<ObjectRef<Snapshot>> {
    let name = repo.name_any();
    select(store, |s: &Snapshot| {
        snapshot_awaiting_deletion(s) && deleting_snapshot_targets_cluster(s, &name)
    })
}

// --- M5 (#345): repository -> PENDING Snapshots (readiness-gate resume) ------

/// Whether a `Snapshot` is parked BEFORE its mover Job launched — the set the
/// repository-readiness gate (`io::repository_ready`, snapshot reconciler) can be
/// holding, which must re-reconcile promptly when its repository recovers instead
/// of waiting out the gate's 15s requeue. Phase `Pending` (or none at all: freshly
/// created), and not deleting (a terminating Snapshot is the ack-drain mappers'
/// business). Exhaustive over [`kopiur_api::snapshot::SnapshotPhase`], so a new
/// phase must decide its membership here before it compiles.
fn snapshot_awaiting_launch(s: &Snapshot) -> bool {
    use kopiur_api::snapshot::SnapshotPhase;
    if s.metadata.deletion_timestamp.is_some() {
        return false;
    }
    match s.status.as_ref().and_then(|st| st.phase) {
        None | Some(SnapshotPhase::Pending) => true,
        Some(
            SnapshotPhase::Running
            | SnapshotPhase::Succeeded
            | SnapshotPhase::Failed
            | SnapshotPhase::Deleting
            | SnapshotPhase::Discovered,
        ) => false,
    }
}

/// [`snapshot_awaiting_launch`] scoped to one namespace — the per-object predicate
/// behind [`repository_to_pending_snapshots`], split out so the matching is
/// unit-testable without a reflector store.
fn pending_snapshot_in_namespace(s: &Snapshot, ns: &str) -> bool {
    snapshot_awaiting_launch(s) && s.namespace().as_deref() == Some(ns)
}

/// PENDING (not-yet-launched) Snapshots potentially gated on the changed namespaced
/// `Repository`, so a repository flipping back to `Ready` resumes them at once
/// instead of on their 15s requeue.
///
/// **Namespace-broadcast (deliberate — precise matching is impossible here).** A
/// gated Snapshot carries NO repository reference of any kind: its spec has only
/// `policyRef` (the repository comes from the policy's `spec.repository`, a
/// two-hop join needing a `SnapshotPolicy` reflector this mapper does not have —
/// same constraint [`repository_to_schedules`] documents), and
/// `status.resolved.repository` is pinned only at mover-Job launch — which is
/// exactly what the readiness gate prevented. Discovered Snapshots do carry a
/// repository ownerRef, but they are never pre-launch (`Discovered` phase) and are
/// excluded by the pending filter anyway. So, mirroring the schedule mapper:
/// policies (and thus their namespace-defaulted repositories) live in the
/// Snapshot's own namespace by construction, so enqueue every pre-launch Snapshot
/// in the changed repository's namespace. Pending Snapshots are few and transient,
/// and a spurious reconcile is one repository GET + a byte-stable no-op patch; the
/// only miss is a policy whose repository ref crosses namespaces, still covered by
/// the gate's own 15s requeue.
pub fn repository_to_pending_snapshots(
    store: &Store<Snapshot>,
    repo: &Repository,
) -> Vec<ObjectRef<Snapshot>> {
    let Some(ns) = repo.namespace() else {
        return Vec::new();
    };
    select(store, |s: &Snapshot| pending_snapshot_in_namespace(s, &ns))
}

/// PENDING Snapshots potentially gated on the changed `ClusterRepository`. A
/// `ClusterRepository` can be referenced from any namespace, so — as with
/// [`cluster_repository_to_schedules`] — the affected namespaces are unbounded and
/// every pre-launch Snapshot cluster-wide is enqueued (same cost/idempotence
/// rationale as [`repository_to_pending_snapshots`]).
pub fn cluster_repository_to_pending_snapshots(
    store: &Store<Snapshot>,
    _repo: &ClusterRepository,
) -> Vec<ObjectRef<Snapshot>> {
    select(store, snapshot_awaiting_launch)
}

// --- Namespace -> privileged-mover-blocked Snapshot / Restore ----------------

/// Whether `conditions` carry a `MoverPermitted != True` entry — the marker a
/// reconciler leaves when it refuses a privileged mover because the namespace
/// has not opted in (`kopiur.home-operations.com/privileged-movers`).
fn mover_blocked(conditions: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition]) -> bool {
    conditions
        .iter()
        .any(|c| c.type_ == crate::consts::MOVER_PERMITTED_CONDITION && c.status != "True")
}

/// Snapshots in the changed `Namespace` that are blocked on the
/// privileged-movers opt-in. Re-enqueued so the annotation takes effect the
/// moment an admin adds it — the blocked CR's own requeue is only a slow
/// watch-desync backstop (`Error::BlockedOnGrant`).
pub fn namespace_to_snapshots(
    store: &Store<Snapshot>,
    namespace: &PartialObjectMeta<Namespace>,
) -> Vec<ObjectRef<Snapshot>> {
    let ns = namespace.name_any();
    select(store, |s: &Snapshot| {
        s.namespace().as_deref() == Some(ns.as_str())
            && s.status
                .as_ref()
                .is_some_and(|st| mover_blocked(&st.conditions))
    })
}

/// Restores in the changed `Namespace` blocked on the privileged-movers opt-in
/// (same contract as [`namespace_to_snapshots`]).
pub fn namespace_to_restores(
    store: &Store<Restore>,
    namespace: &PartialObjectMeta<Namespace>,
) -> Vec<ObjectRef<Restore>> {
    let ns = namespace.name_any();
    select(store, |r: &Restore| {
        r.namespace().as_deref() == Some(ns.as_str())
            && r.status
                .as_ref()
                .is_some_and(|st| mover_blocked(&st.conditions))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kopiur_api::backend::{BackendAuth, FilesystemBackend, S3Backend};
    use kopiur_api::common::{SecretKeyRef, SecretRef};

    fn rref(kind: RepositoryKind, name: &str, ns: Option<&str>) -> RepositoryRef {
        RepositoryRef {
            kind,
            name: name.into(),
            namespace: ns.map(Into::into),
        }
    }

    #[test]
    fn repository_ref_matching_honors_kind_name_and_namespace_default() {
        // Explicit namespace on the ref wins.
        let r = rref(RepositoryKind::Repository, "nas", Some("billing"));
        assert!(ref_targets_repository(&r, Some("other"), "billing", "nas"));
        assert!(!ref_targets_repository(
            &r,
            Some("other"),
            "billing",
            "wrong-name"
        ));
        assert!(!ref_targets_repository(
            &r,
            Some("other"),
            "other-ns",
            "nas"
        ));
        // No explicit namespace → falls back to the consumer's own namespace.
        let r = rref(RepositoryKind::Repository, "nas", None);
        assert!(ref_targets_repository(
            &r,
            Some("billing"),
            "billing",
            "nas"
        ));
        assert!(!ref_targets_repository(
            &r,
            Some("billing"),
            "other-ns",
            "nas"
        ));
        // A ClusterRepository ref never matches a namespaced Repository event.
        let cr = rref(RepositoryKind::ClusterRepository, "nas", None);
        assert!(!ref_targets_repository(
            &cr,
            Some("billing"),
            "billing",
            "nas"
        ));
    }

    #[test]
    fn namespace_mapper_targets_only_mover_blocked_objects() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
        let refused = Condition {
            type_: crate::consts::MOVER_PERMITTED_CONDITION.into(),
            status: "False".into(),
            reason: crate::consts::PRIVILEGED_MOVER_NOT_PERMITTED_REASON.into(),
            message: String::new(),
            last_transition_time: Time(k8s_openapi::jiff::Timestamp::now()),
            observed_generation: None,
        };
        // The refusal marker → mapped (the namespace grant must re-enqueue it).
        assert!(mover_blocked(std::slice::from_ref(&refused)));
        // Permitted (True) or unrelated conditions → not mapped: a namespace
        // event must not re-enqueue every Snapshot in the namespace.
        let permitted = Condition {
            status: "True".into(),
            ..refused.clone()
        };
        assert!(!mover_blocked(&[permitted]));
        let unrelated = Condition {
            type_: "Ready".into(),
            ..refused
        };
        assert!(!mover_blocked(&[unrelated]));
        assert!(!mover_blocked(&[]));
    }

    // --- M4: repository -> deleting Snapshots (ack drain) ----------------

    fn snap(name: &str, deleting: bool, finalizer: bool, pin: Option<RepositoryRef>) -> Snapshot {
        use kopiur_api::snapshot::{ResolvedSnapshot, SnapshotSpec, SnapshotStatus};
        let mut s = Snapshot::new(
            name,
            SnapshotSpec {
                policy_ref: None,
                tags: None,
                failure_policy: None,
                deletion_policy: None,
                on_schedule_delete: None,
                pin: false,
                description: None,
            },
        );
        s.metadata.namespace = Some("media".into());
        if finalizer {
            s.metadata.finalizers =
                Some(vec![crate::consts::SNAPSHOT_CLEANUP_FINALIZER.to_string()]);
        }
        if deleting {
            s.metadata.deletion_timestamp =
                Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                    k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
                ));
        }
        s.status = Some(SnapshotStatus {
            resolved: pin.map(|p| ResolvedSnapshot {
                repository: Some(p),
                sources: vec![],
                credential_projection: None,
            }),
            ..Default::default()
        });
        s
    }

    #[test]
    fn deleting_snapshot_repo_match_pins_unpinned_and_excludes_non_deleting() {
        let pinned_here = rref(RepositoryKind::Repository, "nas", Some("backups"));
        // Pinned to this repo + deleting + finalizer → matched.
        let s = snap("a", true, true, Some(pinned_here.clone()));
        assert!(snapshot_awaiting_deletion(&s));
        assert!(deleting_snapshot_targets_repository(&s, "backups", "nas"));
        // Unpinned + deleting → included (conservative).
        let s = snap("b", true, true, None);
        assert!(deleting_snapshot_targets_repository(&s, "backups", "nas"));
        // Pinned to a DIFFERENT repo → excluded.
        let other = rref(RepositoryKind::Repository, "nas", Some("other-ns"));
        let s = snap("c", true, true, Some(other));
        assert!(!deleting_snapshot_targets_repository(&s, "backups", "nas"));
        // Not deleting, or no finalizer → not an ack-drain candidate.
        assert!(!snapshot_awaiting_deletion(&snap("d", false, true, None)));
        assert!(!snapshot_awaiting_deletion(&snap("e", true, false, None)));
        // A ClusterRepository pin never matches a namespaced Repository event.
        let cpin = rref(RepositoryKind::ClusterRepository, "nas", None);
        let s = snap("f", true, true, Some(cpin));
        assert!(!deleting_snapshot_targets_repository(&s, "backups", "nas"));
    }

    // --- M5 (#345): repository -> pending Snapshots (readiness-gate resume) ---

    fn snap_in_phase(
        name: &str,
        phase: Option<kopiur_api::snapshot::SnapshotPhase>,
        deleting: bool,
    ) -> Snapshot {
        use kopiur_api::snapshot::{SnapshotSpec, SnapshotStatus};
        let mut s = Snapshot::new(
            name,
            SnapshotSpec {
                policy_ref: None,
                tags: None,
                failure_policy: None,
                deletion_policy: None,
                on_schedule_delete: None,
                pin: false,
                description: None,
            },
        );
        s.metadata.namespace = Some("media".into());
        if deleting {
            s.metadata.deletion_timestamp =
                Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                    k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
                ));
        }
        s.status = phase.map(|p| SnapshotStatus {
            phase: Some(p),
            ..Default::default()
        });
        s
    }

    #[test]
    fn pending_snapshot_mapper_matches_only_pre_launch_non_deleting() {
        use kopiur_api::snapshot::SnapshotPhase;
        // Pending — and a freshly created Snapshot with no status at all — are
        // the readiness gate's parking set: matched.
        assert!(snapshot_awaiting_launch(&snap_in_phase(
            "a",
            Some(SnapshotPhase::Pending),
            false
        )));
        assert!(snapshot_awaiting_launch(&snap_in_phase("b", None, false)));
        // Launched / settled phases are never re-gated: skipped.
        for phase in [
            SnapshotPhase::Running,
            SnapshotPhase::Succeeded,
            SnapshotPhase::Failed,
            SnapshotPhase::Deleting,
            SnapshotPhase::Discovered,
        ] {
            assert!(
                !snapshot_awaiting_launch(&snap_in_phase("c", Some(phase), false)),
                "{phase:?} must not map as pending"
            );
        }
        // Deleting (even while the phase still reads Pending) is the ack-drain
        // mappers' business, not this one's.
        assert!(!snapshot_awaiting_launch(&snap_in_phase(
            "d",
            Some(SnapshotPhase::Pending),
            true
        )));
    }

    #[test]
    fn pending_snapshot_mapper_is_a_namespace_broadcast() {
        use kopiur_api::snapshot::SnapshotPhase;
        // A pre-launch Snapshot carries no repository reference (spec has only
        // policyRef; status.resolved.repository is pinned at launch), so the
        // namespaced mapper matches by NAMESPACE, not by ref.
        let s = snap_in_phase("a", Some(SnapshotPhase::Pending), false);
        assert!(pending_snapshot_in_namespace(&s, "media"));
        // A repository in another namespace does not enqueue it.
        assert!(!pending_snapshot_in_namespace(&s, "other"));
        // Non-pending never matches regardless of namespace.
        let s = snap_in_phase("b", Some(SnapshotPhase::Succeeded), false);
        assert!(!pending_snapshot_in_namespace(&s, "media"));
    }

    #[test]
    fn cluster_repository_ref_matching_ignores_namespace() {
        let cr = rref(RepositoryKind::ClusterRepository, "shared", None);
        assert!(ref_targets_cluster(&cr, "shared"));
        assert!(!ref_targets_cluster(&cr, "other"));
        // A namespaced Repository ref never matches a ClusterRepository event.
        let r = rref(RepositoryKind::Repository, "shared", Some("ns"));
        assert!(!ref_targets_cluster(&r, "shared"));
    }

    #[test]
    fn policy_ref_matching_uses_namespace_default() {
        let p = PolicyRef {
            name: "daily".into(),
            namespace: None,
        };
        assert!(ref_targets_policy(&p, Some("apps"), "apps", "daily"));
        assert!(!ref_targets_policy(&p, Some("apps"), "other", "daily"));
        let p = PolicyRef {
            name: "daily".into(),
            namespace: Some("apps".into()),
        };
        assert!(ref_targets_policy(&p, Some("ignored"), "apps", "daily"));
    }

    fn enc(name: &str, ns: Option<&str>) -> Encryption {
        Encryption {
            password_secret_ref: SecretKeyRef {
                name: name.into(),
                namespace: ns.map(Into::into),
                key: None,
            },
        }
    }

    #[test]
    fn repo_references_secret_covers_password_and_backend_auth() {
        // Namespaced repo: password Secret defaults to the repo's namespace.
        let fs = Backend::Filesystem(FilesystemBackend {
            path: "/repo".into(),
            volume: None,
        });
        let e = enc("kopia-creds", None);
        assert!(repo_references_secret(
            &fs,
            &e,
            Some("billing"),
            "billing",
            "kopia-creds"
        ));
        // Wrong namespace / wrong name → no match.
        assert!(!repo_references_secret(
            &fs,
            &e,
            Some("billing"),
            "other",
            "kopia-creds"
        ));
        assert!(!repo_references_secret(
            &fs,
            &e,
            Some("billing"),
            "billing",
            "nope"
        ));

        // S3 backend with a separate auth Secret → both the password and the auth
        // Secret are referents.
        let s3 = Backend::S3(S3Backend {
            bucket: "b".into(),
            prefix: None,
            endpoint: None,
            region: None,
            auth: Some(BackendAuth {
                secret_ref: Some(SecretRef {
                    name: "s3-creds".into(),
                    namespace: None,
                }),
                workload_identity: None,
            }),
            tls: None,
        });
        assert!(repo_references_secret(
            &s3,
            &e,
            Some("billing"),
            "billing",
            "kopia-creds"
        ));
        assert!(repo_references_secret(
            &s3,
            &e,
            Some("billing"),
            "billing",
            "s3-creds"
        ));
        assert!(!repo_references_secret(
            &s3,
            &e,
            Some("billing"),
            "billing",
            "unrelated"
        ));
    }

    #[test]
    fn cluster_repo_references_secret_requires_explicit_namespace() {
        // Cluster-scoped (repo_ns = None): the password ref must pin its own namespace.
        let fs = Backend::Filesystem(FilesystemBackend {
            path: "/repo".into(),
            volume: None,
        });
        let e = enc("kopia-creds", Some("kopia-system"));
        assert!(repo_references_secret(
            &fs,
            &e,
            None,
            "kopia-system",
            "kopia-creds"
        ));
        assert!(!repo_references_secret(
            &fs,
            &e,
            None,
            "elsewhere",
            "kopia-creds"
        ));
        // A ref with no namespace and no repo namespace cannot resolve → never matches.
        let e_no_ns = enc("kopia-creds", None);
        assert!(!repo_references_secret(
            &fs,
            &e_no_ns,
            None,
            "kopia-system",
            "kopia-creds"
        ));
    }
}
