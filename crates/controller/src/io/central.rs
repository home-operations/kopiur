//! **Central execution** (`ClusterRepository.spec.execution.mode: Central`):
//! a backup's mover Job runs in the operator namespace instead of beside its
//! source PVC.
//!
//! A pod can only mount a PVC of its own namespace, so the data crosses over
//! through the one volume primitive that can: the CSI `VolumeSnapshot` is cut in
//! the workload namespace (as `copyMethod: Snapshot` always does), and the staged
//! PVC is provisioned from it in the operator namespace through a cross-namespace
//! `spec.dataSourceRef` (KEP-3294, the alpha `CrossNamespaceVolumeDataSource`
//! feature), authorized by a `ReferenceGrant` the operator creates beside the
//! `VolumeSnapshot`.
//!
//! Two consequences shape everything here:
//!
//! - **No owner references.** An owner must share its dependent's namespace, so
//!   the Job and the staged PVC cannot be owned by the `Snapshot` CR. They carry
//!   [`CENTRAL_SNAPSHOT_UID_LABEL`] instead, the `Snapshot` pins their location in
//!   `status.job` before either exists, and its finalizer deletes them.
//! - **The API server drops what it does not understand.** With the feature gate
//!   off, kube-apiserver clears the whole `dataSourceRef` and admits the claim as
//!   an EMPTY volume: a backup of it would succeed and contain nothing. So the
//!   stored claim is read back and refused unless it still carries the
//!   cross-namespace reference ([`cross_namespace_ref_survived`]).

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{PersistentVolumeClaim, ServiceAccount, TypedObjectReference};
use k8s_openapi::api::rbac::v1::RoleBinding;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::Api;
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};

use kopiur_api::snapshot::JobStatus;
use kopiur_api::{CopyMethod, ExecutionMode};

use super::apply::apply;
use super::child_labels;
use super::mover::{
    MoverRunIdentity, build_mover_rolebinding, build_mover_service_account, derived_mover_name,
    missing_workload_identity_sa_message, wi_rolebinding_named,
};
use super::staging::{SNAPSHOT_GROUP, staged_child_name};
use crate::error::{Error, Result};

/// Label on every object central execution creates outside the `Snapshot`'s
/// namespace, carrying the `Snapshot`'s UID. It stands in for the owner reference
/// those objects cannot have: an object whose label names another `Snapshot` is
/// never adopted, mounted, or deleted on this one's behalf.
pub const CENTRAL_SNAPSHOT_UID_LABEL: &str = "kopiur.home-operations.com/snapshot-uid";
/// Annotation naming the `Snapshot` (`<namespace>/<name>`) a central object
/// belongs to. An annotation because a `Snapshot` name can exceed the 63-byte
/// label-value limit.
pub const CENTRAL_SNAPSHOT_ANNOTATION: &str = "kopiur.home-operations.com/snapshot";

/// The restore-side counterpart of [`CENTRAL_SNAPSHOT_UID_LABEL`].
pub use kopiur_api::consts::CENTRAL_RESTORE_UID_LABEL;
/// Annotation naming the `Restore` (`<namespace>/<name>`) a central object
/// belongs to.
pub const CENTRAL_RESTORE_ANNOTATION: &str = "kopiur.home-operations.com/restore";

/// The policy-side counterpart of [`CENTRAL_SNAPSHOT_UID_LABEL`]: the UID of the
/// `SnapshotPolicy` a central verification Job serves.
pub const CENTRAL_POLICY_UID_LABEL: &str = "kopiur.home-operations.com/policy-uid";
/// Annotation naming the `SnapshotPolicy` (`<namespace>/<name>`) a central
/// object belongs to.
pub const CENTRAL_POLICY_ANNOTATION: &str = "kopiur.home-operations.com/policy";

/// The source cannot be backed up centrally (wrong copy method or source shape).
pub const REASON_CENTRAL_UNSUPPORTED: &str = "CentralExecutionUnsupported";
/// This install cannot run movers centrally (no operator namespace, or a
/// namespaced install).
pub const REASON_CENTRAL_UNAVAILABLE: &str = "CentralExecutionUnavailable";
/// kube-apiserver dropped the staged PVC's cross-namespace `dataSourceRef`.
pub const REASON_CROSS_NAMESPACE_DISABLED: &str = "CrossNamespaceDataSourceDisabled";
/// The Gateway API `ReferenceGrant` CRD is not installed.
pub const REASON_REFERENCE_GRANT_MISSING: &str = "ReferenceGrantCRDMissing";
/// A central object of the derived name belongs to a different `Snapshot`.
pub const REASON_CENTRAL_NAME_CONFLICT: &str = "CentralObjectNameConflict";

/// See [`MoverSite::with_bind_hint`].
const CENTRAL_BIND_HINT: &str = "Under `execution.mode: Central` a staged PVC that never binds \
     usually means the CSI driver's external-provisioner runs without \
     `--feature-gates=CrossNamespaceVolumeDataSource=true`, or lacks `get`/`list`/`watch` on \
     `referencegrants.gateway.networking.k8s.io` (see the PVC's events)";

/// Distinguishing suffix of the dedicated central mover identity, as the chart's
/// `kopiur.centralMoverName` helper appends it.
const CENTRAL_MOVER_SUFFIX: &str = "central-mover";

/// The Gateway API group `ReferenceGrant` lives in.
const GATEWAY_GROUP: &str = "gateway.networking.k8s.io";

/// The dedicated central mover identity's name, derived from the generic mover
/// role name the same way [`super::stream_mover_name`] is. Keep in lockstep with
/// the chart's `kopiur.centralMoverName` helper.
pub fn central_mover_name(base: &str) -> String {
    derived_mover_name(base, CENTRAL_MOVER_SUFFIX)
}

/// Where a backup's mover Job, its pods, and its staged PVC live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoverSite {
    /// The `Snapshot`'s own namespace; the Job is named after the `Snapshot` and
    /// owner-referenced to it.
    InNamespace,
    /// The operator namespace (`execution.mode: Central`).
    Central(CentralSite),
}

/// The operator-namespace location of a centrally executed backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CentralSite {
    /// The namespace the Job and staged PVC live in.
    pub namespace: String,
    /// The mover Job's name ([`central_job_name`]).
    pub job_name: String,
}

impl MoverSite {
    /// The site a run already committed to, read from `status.job`. A run with no
    /// pinned namespace is in-namespace: the pin is written before any central
    /// object is created, so its absence proves none exists.
    pub fn pinned(job: Option<&JobStatus>, snapshot_ns: &str, snapshot_name: &str) -> MoverSite {
        match job.and_then(|j| j.namespace.as_ref()) {
            Some(namespace) => MoverSite::Central(CentralSite {
                namespace: namespace.clone(),
                job_name: job
                    .and_then(|j| j.name.clone())
                    .unwrap_or_else(|| central_job_name(snapshot_ns, snapshot_name)),
            }),
            None => MoverSite::InNamespace,
        }
    }

    /// The namespace of the mover Job, its pods, and the staged PVC.
    pub fn namespace<'a>(&'a self, snapshot_ns: &'a str) -> &'a str {
        match self {
            MoverSite::InNamespace => snapshot_ns,
            MoverSite::Central(site) => &site.namespace,
        }
    }

    /// The mover Job's name.
    pub fn job_name(&self, snapshot_name: &str) -> String {
        match self {
            MoverSite::InNamespace => crate::naming::capped_job_name(snapshot_name),
            MoverSite::Central(site) => site.job_name.clone(),
        }
    }

    /// The staged PVC's name.
    pub fn staged_pvc_name(&self, snapshot_name: &str) -> String {
        match self {
            MoverSite::InNamespace => super::staging::staged_pvc_name(snapshot_name),
            MoverSite::Central(site) => staged_child_name(&site.job_name, "src"),
        }
    }

    /// A staged-PVC bind failure `message`, with what usually causes it under
    /// central execution appended: the provisioner side of the cross-namespace
    /// feature, which the operator cannot observe directly. Unchanged in-namespace.
    pub fn with_bind_hint(&self, message: String) -> String {
        match self {
            MoverSite::InNamespace => message,
            MoverSite::Central(_) => format!("{message}. {CENTRAL_BIND_HINT}"),
        }
    }

    /// Whether an object found under one of this run's names belongs to some
    /// OTHER run, so this run must neither use nor delete it.
    ///
    /// Central and in-namespace runs share the operator namespace, and a central
    /// object's name is a legal `Snapshot` name. So the check runs both ways: a
    /// central run owns only objects labelled with its UID, and an in-namespace
    /// run owns no object that carries the central label at all.
    pub fn is_foreign(&self, meta: &kube::core::ObjectMeta, uid: &str) -> bool {
        match self {
            MoverSite::InNamespace => has_central_mark(meta),
            MoverSite::Central(_) => !is_ours(meta, uid),
        }
    }

    /// Whether this run may have staged objects to reap. In-namespace that is
    /// what `status.staged` records (`staged_recorded`). A central run counts from
    /// its pin on: its staged PVC is created before `status.staged` is stamped
    /// and has no owner reference, so nothing else would ever reap it.
    pub fn may_have_stage(&self, staged_recorded: bool) -> bool {
        match self {
            MoverSite::InNamespace => staged_recorded,
            MoverSite::Central(_) => true,
        }
    }

    /// The `status.job` pin recording this site, `None` in-namespace (where the
    /// status stays byte-identical to a build without central execution).
    pub fn status_pin(&self) -> Option<serde_json::Value> {
        match self {
            MoverSite::InNamespace => None,
            MoverSite::Central(site) => Some(serde_json::json!({
                "name": site.job_name,
                "namespace": site.namespace,
            })),
        }
    }
}

/// The central mover Job's name for the `Snapshot` `snapshot_ns/snapshot_name`:
/// a legible prefix of the `Snapshot` name plus the full 64-bit FNV-1a of
/// `<namespace>/<name>`. The hash is ALWAYS present: two namespaces may each
/// hold a `Snapshot` of the same name, and both Jobs land in one namespace.
/// Bounded by [`kopiur_api::consts::JOB_NAME_MAX`].
pub fn central_job_name(snapshot_ns: &str, snapshot_name: &str) -> String {
    let hash = crate::naming::wide_hash(&format!("{snapshot_ns}/{snapshot_name}"));
    let keep = kopiur_api::consts::JOB_NAME_MAX - (hash.len() + 1);
    let prefix: String = snapshot_name.chars().take(keep).collect();
    format!("{}-{hash}", prefix.trim_end_matches(['-', '.']))
}

/// Why central execution cannot run at all on this install, if it cannot.
/// `operator_namespace` is `KOPIUR_NAMESPACE`; `namespaced_install` is a
/// Role-scoped install, whose RBAC reaches one namespace only.
pub fn central_site_for(
    operator_namespace: Option<&str>,
    namespaced_install: bool,
    snapshot_ns: &str,
    snapshot_name: &str,
) -> std::result::Result<CentralSite, (&'static str, String)> {
    let namespace = central_namespace(operator_namespace, namespaced_install).map_err(
        |(reason, message)| {
            (
                reason,
                format!("{message}. The next scheduled run (or a new Snapshot) retries"),
            )
        },
    )?;
    Ok(CentralSite {
        namespace,
        job_name: central_job_name(snapshot_ns, snapshot_name),
    })
}

/// The namespace central movers run in, or why this install has none. The
/// message is a full sentence without its final period, so a caller can append
/// what happens next.
pub fn central_namespace(
    operator_namespace: Option<&str>,
    namespaced_install: bool,
) -> std::result::Result<String, (&'static str, String)> {
    if namespaced_install {
        return Err((
            REASON_CENTRAL_UNAVAILABLE,
            "the repository sets `execution.mode: Central`, but this is a namespaced install \
             (`installScope: namespaced`), whose Role cannot create objects in a second \
             namespace. Fix: use a cluster-scoped install, or remove `execution` from the \
             ClusterRepository"
                .to_string(),
        ));
    }
    match operator_namespace.filter(|ns| !ns.is_empty()) {
        Some(namespace) => Ok(namespace.to_string()),
        None => Err((
            REASON_CENTRAL_UNAVAILABLE,
            "the repository sets `execution.mode: Central`, but the operator does not know its own \
             namespace. Fix: set the `KOPIUR_NAMESPACE` environment variable on the controller \
             (the Helm chart sets it), or remove `execution` from the ClusterRepository"
                .to_string(),
        )),
    }
}

/// Where a mover Job that reads only the repository runs: a verification or a
/// pin. Such a Job mounts no workload volume, so under `execution.mode:
/// Central` it needs nothing from the workload namespace, and the repository
/// credentials stay out of it.
///
/// Unlike a backup, the site is not pinned in status. These Jobs hold no data:
/// if the repository's `execution` changes while one runs, the Job left at the
/// old site finishes or expires by its TTL, and the next reconcile starts the
/// work again at the new site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoJobSite {
    /// In the owning CR's namespace, with an owner reference to it.
    InNamespace,
    /// In the operator namespace, marked with the owner's UID.
    Central {
        /// The operator namespace.
        namespace: String,
    },
}

impl RepoJobSite {
    /// The site the repository asks for. Only a `ClusterRepository` can ask for
    /// `Central` (a namespaced `Repository` has no `execution` field).
    pub fn of(
        kind: kopiur_api::common::RepositoryKind,
        mode: ExecutionMode,
        operator_namespace: Option<&str>,
        namespaced_install: bool,
    ) -> std::result::Result<Self, (&'static str, String)> {
        use kopiur_api::common::RepositoryKind;
        match (kind, mode) {
            (RepositoryKind::ClusterRepository, ExecutionMode::Central) => {
                central_namespace(operator_namespace, namespaced_install)
                    .map(|namespace| RepoJobSite::Central { namespace })
            }
            (RepositoryKind::ClusterRepository, ExecutionMode::InNamespace)
            | (RepositoryKind::Repository, _) => Ok(RepoJobSite::InNamespace),
        }
    }

    /// The namespace the Job lives in, given its owner's namespace.
    pub fn namespace<'a>(&'a self, owner_ns: &'a str) -> &'a str {
        match self {
            RepoJobSite::InNamespace => owner_ns,
            RepoJobSite::Central { namespace } => namespace,
        }
    }

    /// Whether the mover runs outside its owner's namespace.
    pub fn is_central(&self) -> bool {
        matches!(self, RepoJobSite::Central { .. })
    }

    /// Whether a Job found under this run's name, or by its selector, belongs
    /// to someone else. `carries_owner_uid` is whether it carries this owner's
    /// UID label. Central and in-namespace Jobs share the operator namespace,
    /// so an in-namespace owner must never act on a central Job, and a central
    /// owner only on a Job that names it.
    pub fn is_foreign(&self, meta: &kube::core::ObjectMeta, carries_owner_uid: bool) -> bool {
        match self {
            RepoJobSite::InNamespace => has_central_mark(meta),
            RepoJobSite::Central { .. } => !carries_owner_uid,
        }
    }
}

/// Mark `meta` as a central object of the `SnapshotPolicy`
/// `policy_ns/policy_name` (UID `uid`), and drop the owner reference a
/// cross-namespace object must not carry. See [`mark_central`].
pub fn mark_central_policy(
    meta: &mut kube::core::ObjectMeta,
    policy_ns: &str,
    policy_name: &str,
    uid: &str,
) {
    meta.owner_references = None;
    meta.labels
        .get_or_insert_with(BTreeMap::new)
        .insert(CENTRAL_POLICY_UID_LABEL.to_string(), uid.to_string());
    meta.annotations.get_or_insert_with(BTreeMap::new).insert(
        CENTRAL_POLICY_ANNOTATION.to_string(),
        format!("{policy_ns}/{policy_name}"),
    );
}

/// Whether a central object belongs to the `SnapshotPolicy` with UID `uid`.
pub fn is_policys(meta: &kube::core::ObjectMeta, uid: &str) -> bool {
    meta.labels
        .as_ref()
        .and_then(|l| l.get(CENTRAL_POLICY_UID_LABEL))
        .is_some_and(|v| v == uid)
}

/// Delete every central Job of the `SnapshotPolicy` with UID `policy_uid` (its
/// verification Jobs; their pods cascade). Called from the policy's finalizer,
/// because no owner reference reaches these Jobs. Idempotent and 404-tolerant.
pub async fn reap_central_policy_jobs(
    client: &kube::Client,
    central_ns: &str,
    policy_uid: &str,
) -> Result<()> {
    let selector = format!("{CENTRAL_POLICY_UID_LABEL}={policy_uid}");
    let params = kube::api::ListParams::default().labels(&selector);
    let jobs: Api<k8s_openapi::api::batch::v1::Job> = Api::namespaced(client.clone(), central_ns);
    for job in jobs.list(&params).await?.items {
        match jobs
            .delete(
                &kube::ResourceExt::name_any(&job),
                &delete_exactly(&job.metadata),
            )
            .await
        {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => {}
            Err(e) => return Err(Error::Kube(e)),
        }
    }
    Ok(())
}

/// Where a central mover pod mounts its source volume, whatever the source is.
///
/// A central mover pod holds the shared repository credentials, so nothing a
/// tenant writes may decide where a volume is mounted in it: a volume mounted
/// over the mover's own binaries would run tenant code with those credentials.
/// The path kopia RECORDS is separate (the identity's `sourcePath`, passed as
/// `--override-source`), so a `sourcePathOverride` and the default
/// `/pvc/<name>` are recorded exactly as an in-namespace run records them.
pub const CENTRAL_SOURCE_MOUNT_PATH: &str = "/source";

/// **Pure.** Whether two absolute mount paths overlap: they are the same
/// directory, or one is inside the other. Compared by whole path segments, so
/// `/source` and `/sources` do not overlap. Empty and `.` segments are
/// dropped first (`/./source//repo` is `/source/repo`). A path with a `..`
/// segment is not resolved: it is reported as overlapping, because what it
/// names depends on the directories around it.
pub fn mount_paths_overlap(a: &str, b: &str) -> bool {
    let segments = |p: &str| -> Vec<String> {
        p.split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .map(str::to_string)
            .collect()
    };
    let (a, b) = (segments(a), segments(b));
    if a.iter().chain(&b).any(|s| s == "..") {
        return true;
    }
    let shared = a.len().min(b.len());
    a[..shared] == b[..shared]
}

/// Why a filesystem repository mounted at `repo_path` cannot serve a central
/// backup, if it cannot: the mover pod mounts the repository volume there and
/// the source at [`CENTRAL_SOURCE_MOUNT_PATH`], and Kubernetes rejects a pod
/// whose two mounts overlap.
pub fn central_repo_mount_refusal(
    repo_path: Option<&str>,
    repository: &str,
) -> Option<(&'static str, String)> {
    let repo_path = repo_path.filter(|p| mount_paths_overlap(p, CENTRAL_SOURCE_MOUNT_PATH))?;
    Some((
        REASON_CENTRAL_UNSUPPORTED,
        format!(
            "ClusterRepository `{repository}` sets `execution.mode: Central` and keeps its \
             filesystem repository at `{repo_path}`, which overlaps `{CENTRAL_SOURCE_MOUNT_PATH}`, \
             the path a central mover pod mounts its source at. Fix: use a filesystem path \
             outside `{CENTRAL_SOURCE_MOUNT_PATH}` for the repository, or remove `execution` \
             from the ClusterRepository. The next scheduled run (or a new Snapshot) retries"
        ),
    ))
}

/// The shape of the source one run backs up, as far as central execution cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CentralSource {
    /// A single PVC.
    Pvc,
    /// A PVC that is a member of a `groupBy: VolumeGroupSnapshot` consistency
    /// group.
    GroupMember,
    /// An NFS export, which the mover pod itself would mount.
    Nfs,
    /// A `stream` source (the mover execs into a workload pod).
    Stream,
    /// Nothing to back up: a `pvcSelector` source with no PVC pinned.
    Nothing,
}

impl CentralSource {
    /// Classify the source one run covers. `grouped` is whether the `Snapshot`
    /// is a member of a consistency group.
    pub fn of(source: &kopiur_api::expand::EffectiveSource, grouped: bool) -> CentralSource {
        if source.stream.is_some() {
            CentralSource::Stream
        } else if source.pvc.is_some() {
            if grouped {
                CentralSource::GroupMember
            } else {
                CentralSource::Pvc
            }
        } else if source.nfs_path.is_some() {
            CentralSource::Nfs
        } else {
            CentralSource::Nothing
        }
    }
}

/// Why this source cannot be backed up centrally, if it cannot. A PVC crosses
/// namespaces as a CSI snapshot or a CSI clone. What is left cannot leave the
/// workload namespace, or must not, and refusing it is what keeps a `Central`
/// repository from quietly running a mover there after all.
pub fn central_source_refusal(
    copy_method: CopyMethod,
    source: CentralSource,
    repository: &str,
) -> Option<(&'static str, String)> {
    let why = match (source, copy_method) {
        (
            CentralSource::Pvc | CentralSource::GroupMember,
            CopyMethod::Snapshot | CopyMethod::Clone,
        ) => return None,
        // Technically a pod in any namespace can mount an NFS export. That is
        // the problem: the workload namespace's Pod Security level may forbid
        // `nfs` volumes (the `restricted` level does), and a mover pod in the
        // operator namespace is not subject to it. A tenant could have it mount
        // any export the nodes can reach, then restore that data to themselves.
        (CentralSource::Nfs, _) => {
            "an NFS source is mounted by the mover pod itself, and in the operator namespace \
             that mount would bypass the Pod Security rules of the workload namespace"
        }
        (CentralSource::Pvc | CentralSource::GroupMember, CopyMethod::Direct) => {
            "`copyMethod: Direct` mounts the live source PVC, and a pod can only mount a PVC of \
             its own namespace"
        }
        (CentralSource::Stream, _) => {
            "a `stream` source runs a command inside a workload pod, which a central mover does \
             not do"
        }
        (CentralSource::Nothing, _) => "the source names no PVC and no NFS export",
    };
    Some((
        REASON_CENTRAL_UNSUPPORTED,
        format!(
            "ClusterRepository `{repository}` sets `execution.mode: Central`, which backs up a \
             PVC under `copyMethod: Snapshot` or `Clone`; {why}. Fix: change \
             the source or the copy method, or point this SnapshotPolicy at a repository \
             without `execution.mode: Central`. The next scheduled run (or a new Snapshot) \
             retries"
        ),
    ))
}

/// The object in the workload namespace that a central staged PVC is filled
/// from, through a cross-namespace `dataSourceRef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossNamespaceSource<'a> {
    /// A CSI `VolumeSnapshot` of the source PVC (`copyMethod: Snapshot`).
    VolumeSnapshot(&'a str),
    /// The source PVC itself, as a CSI clone (`copyMethod: Clone`).
    Pvc(&'a str),
}

impl CrossNamespaceSource<'_> {
    /// The API group, as a `ReferenceGrant` writes it (`""` is the core group).
    pub fn group(&self) -> &'static str {
        match self {
            CrossNamespaceSource::VolumeSnapshot(_) => SNAPSHOT_GROUP,
            CrossNamespaceSource::Pvc(_) => "",
        }
    }

    /// The kind.
    pub fn kind(&self) -> &'static str {
        match self {
            CrossNamespaceSource::VolumeSnapshot(_) => "VolumeSnapshot",
            CrossNamespaceSource::Pvc(_) => "PersistentVolumeClaim",
        }
    }

    /// The object's name.
    pub fn name(&self) -> &str {
        match self {
            CrossNamespaceSource::VolumeSnapshot(name) | CrossNamespaceSource::Pvc(name) => name,
        }
    }

    /// The `dataSourceRef` a claim in another namespace uses to name this
    /// object in `source_ns`. The core group is written as an absent
    /// `apiGroup`, which is how the API server and the CSI provisioner read it.
    pub fn data_source_ref(&self, source_ns: &str) -> TypedObjectReference {
        TypedObjectReference {
            api_group: match self {
                CrossNamespaceSource::VolumeSnapshot(_) => Some(SNAPSHOT_GROUP.to_string()),
                CrossNamespaceSource::Pvc(_) => None,
            },
            kind: self.kind().to_string(),
            name: self.name().to_string(),
            namespace: Some(source_ns.to_string()),
        }
    }
}

/// The site a run that has not launched yet should use. A pinned site always
/// wins. An unpinned run that already staged in its own namespace stays there:
/// the repository's `execution` was switched mid-run, and moving now would
/// strand that stage.
pub fn wants_central(pinned: &MoverSite, mode: ExecutionMode, already_staged: bool) -> bool {
    match (pinned, mode) {
        (MoverSite::Central(_), _) => true,
        (MoverSite::InNamespace, ExecutionMode::Central) => !already_staged,
        (MoverSite::InNamespace, ExecutionMode::InNamespace) => false,
    }
}

/// Labels + annotations marking `meta` as belonging to the `Snapshot`
/// `snapshot_ns/snapshot_name` (UID `uid`), and dropping the owner reference a
/// cross-namespace object must not carry (the garbage collector treats an owner
/// it cannot find in the dependent's namespace as gone and deletes the dependent).
pub fn mark_central(
    meta: &mut kube::core::ObjectMeta,
    snapshot_ns: &str,
    snapshot_name: &str,
    uid: &str,
) {
    meta.owner_references = None;
    meta.labels
        .get_or_insert_with(BTreeMap::new)
        .insert(CENTRAL_SNAPSHOT_UID_LABEL.to_string(), uid.to_string());
    meta.annotations.get_or_insert_with(BTreeMap::new).insert(
        CENTRAL_SNAPSHOT_ANNOTATION.to_string(),
        format!("{snapshot_ns}/{snapshot_name}"),
    );
}

/// Whether a central object found under this run's derived name is this run's.
/// The name embeds a 64-bit hash of `<namespace>/<name>`, so a mismatch means a
/// hash collision or a recreated `Snapshot` whose predecessor's object is still
/// terminating: either way it holds someone else's data.
pub fn is_ours(meta: &kube::core::ObjectMeta, uid: &str) -> bool {
    meta.labels
        .as_ref()
        .and_then(|l| l.get(CENTRAL_SNAPSHOT_UID_LABEL))
        .is_some_and(|v| v == uid)
}

/// Whether `meta` is a central object of ANY run, backup or restore. No
/// in-namespace run owns such an object, whatever its name.
pub fn has_central_mark(meta: &kube::core::ObjectMeta) -> bool {
    meta.labels.as_ref().is_some_and(|l| {
        l.contains_key(CENTRAL_SNAPSHOT_UID_LABEL)
            || l.contains_key(CENTRAL_RESTORE_UID_LABEL)
            || l.contains_key(CENTRAL_POLICY_UID_LABEL)
    })
}

/// The message for a central object that belongs to another `Snapshot`.
pub fn name_conflict_message(kind: &str, namespace: &str, name: &str) -> String {
    format!(
        "{kind} `{namespace}/{name}` already exists and belongs to a different Snapshot, so this \
         backup will not use it. It is usually the remains of a deleted Snapshot of the same name \
         that is still being cleaned up. Fix: wait for it to be deleted (or delete it), then \
         create a new Snapshot"
    )
}

/// Turn a staged PVC built for the `Snapshot`'s own namespace into its central
/// form: the data source becomes a cross-namespace `dataSourceRef` to `source`
/// in `source_ns`, and ownership moves from an owner reference to the central
/// marks. `dataSource` is cleared because the API server rejects a claim that
/// sets it together with `dataSourceRef.namespace`.
pub fn centralize_staged_pvc(
    pvc: &mut PersistentVolumeClaim,
    source_ns: &str,
    source: CrossNamespaceSource<'_>,
    snapshot_name: &str,
    uid: &str,
) {
    mark_central(&mut pvc.metadata, source_ns, snapshot_name, uid);
    if let Some(spec) = pvc.spec.as_mut() {
        spec.data_source = None;
        spec.data_source_ref = Some(source.data_source_ref(source_ns));
    }
}

/// Whether the claim the API server STORED still points at `source` in
/// `source_ns`. `false` means the `CrossNamespaceVolumeDataSource` feature gate
/// is off on kube-apiserver, which clears the reference and leaves a claim for
/// an empty volume.
pub fn cross_namespace_ref_survived(
    stored: &PersistentVolumeClaim,
    source_ns: &str,
    source: CrossNamespaceSource<'_>,
) -> bool {
    stored
        .spec
        .as_ref()
        .and_then(|s| s.data_source_ref.as_ref())
        .is_some_and(|r| {
            r.kind == source.kind()
                && r.name == source.name()
                && r.namespace.as_deref() == Some(source_ns)
        })
}

/// The message for a staged PVC whose cross-namespace reference was dropped.
pub fn cross_namespace_disabled_message(namespace: &str, pvc: &str) -> String {
    format!(
        "kube-apiserver dropped the cross-namespace `dataSourceRef` of staged PVC \
         `{namespace}/{pvc}`, so it would have been an empty volume; the PVC was deleted and no \
         backup was taken. The `CrossNamespaceVolumeDataSource` feature gate is off. Fix: enable \
         `CrossNamespaceVolumeDataSource=true` on kube-apiserver and kube-controller-manager, and \
         pass `--feature-gates=CrossNamespaceVolumeDataSource=true` to the CSI driver's \
         external-provisioner; or remove `execution` from the ClusterRepository. The next \
         scheduled run (or a new Snapshot) retries"
    )
}

/// The message for a cluster without the `ReferenceGrant` CRD.
pub fn reference_grant_missing_message() -> String {
    "central execution authorizes the cross-namespace snapshot restore with a Gateway API \
     `ReferenceGrant`, but `referencegrants.gateway.networking.k8s.io` (v1beta1) is not installed. \
     Fix: install the Gateway API CRDs, and grant the CSI driver's external-provisioner `get`, \
     `list`, `watch` on `referencegrants`; or remove `execution` from the ClusterRepository. The \
     next scheduled run (or a new Snapshot) retries"
        .to_string()
}

/// `ApiResource` for the Gateway API `ReferenceGrant` (namespaced). `v1beta1` is
/// the version the CSI external-provisioner reads.
pub fn reference_grant_ar() -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(GATEWAY_GROUP, "v1beta1", "ReferenceGrant"),
        "referencegrants",
    )
}

/// The `ReferenceGrant` name for a `Snapshot` CR.
pub fn reference_grant_name(snapshot_cr: &str) -> String {
    staged_child_name(snapshot_cr, "grant")
}

/// Build the `ReferenceGrant` that lets PVCs of `central_ns` be filled from the
/// ONE object `source` in `ns`. It lives beside that object, owner-referenced
/// to the `Snapshot` CR so it is reaped with it.
pub fn build_reference_grant(
    name: &str,
    ns: &str,
    central_ns: &str,
    source: CrossNamespaceSource<'_>,
    owner: OwnerReference,
) -> DynamicObject {
    let mut obj = DynamicObject::new(name, &reference_grant_ar())
        .within(ns)
        .data(serde_json::json!({
            "spec": {
                "from": [{
                    "group": "",
                    "kind": "PersistentVolumeClaim",
                    "namespace": central_ns,
                }],
                "to": [{
                    "group": source.group(),
                    "kind": source.kind(),
                    "name": source.name(),
                }],
            }
        }));
    obj.metadata.owner_references = Some(vec![owner]);
    obj.metadata.labels = Some(child_labels(&[(
        "kopiur.home-operations.com/component",
        "staged-source-grant",
    )]));
    obj
}

/// The `ReferenceGrant` API in `ns`.
pub fn reference_grant_api(client: &kube::Client, ns: &str) -> Api<DynamicObject> {
    Api::namespaced_with(client.clone(), ns, &reference_grant_ar())
}

/// Build the `RoleBinding` in the WORKLOAD namespace `workload_ns` that lets the
/// central mover's ServiceAccount (`sa_name` in `central_ns`) patch the status of
/// the `Snapshot` it backs up. The role carries `snapshots/status` only.
pub fn build_central_status_binding(
    workload_ns: &str,
    binding_name: &str,
    sa_name: &str,
    central_ns: &str,
    role_name: &str,
) -> RoleBinding {
    let mut rb = build_mover_rolebinding(workload_ns, sa_name, "ClusterRole", role_name);
    rb.metadata.name = Some(binding_name.to_string());
    if let Some(subject) = rb.subjects.as_mut().and_then(|s| s.first_mut()) {
        subject.namespace = Some(central_ns.to_string());
    }
    rb
}

/// Resolve the identity a CENTRAL mover Job runs as and ensure its RBAC: the
/// central sibling of [`super::ensure_mover_identity`].
///
/// The Job runs in `central_ns` but patches a `Snapshot` in `workload_ns`, so the
/// ServiceAccount lives in the former and its RoleBinding in the latter. It is a
/// dedicated `…-central-mover` identity bound to an equally dedicated role that
/// holds `snapshots/status` and nothing else: one ServiceAccount serves every
/// centrally backed-up namespace, so it must not accumulate the generic mover
/// role's `configmaps` verbs across all of them.
///
/// A workload-identity backend names the user's ServiceAccount instead, which
/// must exist in `central_ns` (the pod runs there).
pub async fn ensure_central_mover_identity(
    client: &kube::Client,
    central_ns: &str,
    workload_ns: &str,
    backends: &[&kopiur_api::backend::Backend],
    mover_role_base: &str,
) -> Result<MoverRunIdentity> {
    use kopiur_api::creds::{WorkloadIdentityCloud, backend_workload_identity};
    let dedicated = central_mover_name(mover_role_base);
    let rb_api: Api<RoleBinding> = Api::namespaced(client.clone(), workload_ns);
    let sa_api: Api<ServiceAccount> = Api::namespaced(client.clone(), central_ns);
    let wi: Vec<_> = backends
        .iter()
        .filter_map(|b| backend_workload_identity(b))
        .collect();
    let Some((first, first_cloud)) = wi.first() else {
        let sa = build_mover_service_account(central_ns, &dedicated);
        apply(&sa_api, &dedicated, &sa).await?;
        let rb = build_central_status_binding(
            workload_ns,
            &dedicated,
            &dedicated,
            central_ns,
            &dedicated,
        );
        apply(&rb_api, &dedicated, &rb).await?;
        return Ok(MoverRunIdentity {
            service_account: Some(dedicated),
            azure_workload_identity: false,
        });
    };
    let sa_name = first.service_account_name.clone();
    let azure = wi
        .iter()
        .any(|(_, cloud)| *cloud == WorkloadIdentityCloud::Azure);
    if sa_api
        .get_opt(&sa_name)
        .await
        .map_err(Error::Kube)?
        .is_none()
    {
        return Err(Error::MissingDependency(
            missing_workload_identity_sa_message(
                &sa_name,
                central_ns,
                *first_cloud,
                super::mover::WI_CONSUMER_MOVER,
            ),
        ));
    }
    let rb_name = wi_rolebinding_named("kopiur-central-mover-wi-", &sa_name);
    let rb = build_central_status_binding(workload_ns, &rb_name, &sa_name, central_ns, &dedicated);
    apply(&rb_api, &rb_name, &rb).await?;
    Ok(MoverRunIdentity {
        service_account: Some(sa_name),
        azure_workload_identity: azure,
    })
}

/// Delete a central mover Job (and, by cascade, its pods) on `Snapshot` deletion.
/// Idempotent and 404-tolerant; a Job of that name that belongs to another
/// `Snapshot` is left alone.
pub async fn delete_central_job(
    client: &kube::Client,
    site: &CentralSite,
    uid: &str,
) -> Result<()> {
    let jobs: Api<k8s_openapi::api::batch::v1::Job> =
        Api::namespaced(client.clone(), &site.namespace);
    let Some(job) = jobs.get_opt(&site.job_name).await? else {
        return Ok(());
    };
    if !is_ours(&job.metadata, uid) {
        return Ok(());
    }
    match jobs
        .delete(&site.job_name, &kube::api::DeleteParams::background())
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(Error::Kube(e)),
    }
}

/// Where a populator restore's prime PVCs and mover Jobs live.
///
/// A populator restore fills a temporary "prime" PVC and then hands its
/// `PersistentVolume` to the PVC that claimed the `Restore`. A
/// `PersistentVolume` is cluster-scoped, so the prime PVC and the mover need not
/// be in the claim's namespace: under central execution they are in the operator
/// namespace, and nothing but the finished volume enters the workload namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreSite {
    /// The `Restore`'s own namespace; artifacts are owner-referenced to it.
    InNamespace,
    /// The operator namespace (`execution.mode: Central`).
    Central {
        /// The namespace the prime PVCs and mover Jobs live in.
        namespace: String,
    },
}

impl RestoreSite {
    /// The site a restore already committed to, read from `status.execution`.
    /// No pin means in-namespace: the pin is written before any central object
    /// is created.
    pub fn pinned(status: Option<&kopiur_api::RestoreStatus>) -> RestoreSite {
        match status.and_then(|s| s.execution.as_ref()) {
            Some(execution) => RestoreSite::Central {
                namespace: execution.namespace.clone(),
            },
            None => RestoreSite::InNamespace,
        }
    }

    /// The namespace of the prime PVCs, the mover Jobs and their pods.
    pub fn namespace<'a>(&'a self, restore_ns: &'a str) -> &'a str {
        match self {
            RestoreSite::InNamespace => restore_ns,
            RestoreSite::Central { namespace } => namespace,
        }
    }

    /// Whether an object found under one of this restore's names belongs to
    /// some other run (see [`MoverSite::is_foreign`], whose rule this mirrors).
    pub fn is_foreign(&self, meta: &kube::core::ObjectMeta, restore_uid: &str) -> bool {
        match self {
            RestoreSite::InNamespace => has_central_mark(meta),
            RestoreSite::Central { .. } => !is_restores(meta, restore_uid),
        }
    }
}

/// Whether a central object carries this `Restore`'s UID.
pub fn is_restores(meta: &kube::core::ObjectMeta, restore_uid: &str) -> bool {
    meta.labels
        .as_ref()
        .and_then(|l| l.get(CENTRAL_RESTORE_UID_LABEL))
        .is_some_and(|v| v == restore_uid)
}

/// Mark `meta` as a central object of the `Restore` `restore_ns/restore_name`
/// and drop the owner reference a cross-namespace object must not carry.
pub fn mark_central_restore(
    meta: &mut kube::core::ObjectMeta,
    restore_ns: &str,
    restore_name: &str,
    restore_uid: &str,
) {
    meta.owner_references = None;
    meta.labels.get_or_insert_with(BTreeMap::new).insert(
        CENTRAL_RESTORE_UID_LABEL.to_string(),
        restore_uid.to_string(),
    );
    meta.annotations.get_or_insert_with(BTreeMap::new).insert(
        CENTRAL_RESTORE_ANNOTATION.to_string(),
        format!("{restore_ns}/{restore_name}"),
    );
}

/// Whether a populator restore that has not committed to a site yet runs
/// centrally. A pinned restore stays central. An unpinned one that already
/// created artifacts beside its claims stays there: the repository's
/// `execution` was switched afterwards, and moving now would strand them.
pub fn restore_wants_central(
    pinned: &RestoreSite,
    mode: ExecutionMode,
    started_in_namespace: bool,
) -> bool {
    match (pinned, mode) {
        (RestoreSite::Central { .. }, _) => true,
        (RestoreSite::InNamespace, ExecutionMode::Central) => !started_in_namespace,
        (RestoreSite::InNamespace, ExecutionMode::InNamespace) => false,
    }
}

/// Why a run pinned to a central site cannot launch against `kind`, if it
/// cannot. `subject`/`name` name the run's object for the message.
///
/// A central mover takes the repository's credential Secrets and a filesystem
/// repository's PVC from the operator namespace BY NAME. Those names must come
/// from a `ClusterRepository`, which only a cluster administrator can write. A
/// namespaced `Repository` is tenant-authored: honoring one here would let a
/// tenant point a mover at any Secret or PVC in the operator namespace. The pin
/// outlives edits of the run's repository reference, so this is checked at
/// every launch, not only when the site is chosen.
pub fn central_repository_refusal(
    site: &RestoreSite,
    kind: kopiur_api::common::RepositoryKind,
    subject: &str,
    name: &str,
) -> Option<String> {
    use kopiur_api::common::RepositoryKind;
    match (site, kind) {
        (RestoreSite::InNamespace, _)
        | (RestoreSite::Central { .. }, RepositoryKind::ClusterRepository) => None,
        (RestoreSite::Central { namespace }, RepositoryKind::Repository) => Some(format!(
            "{subject} `{name}` runs its mover centrally, in namespace `{namespace}`, but its \
             repository now resolves to a namespaced Repository. Central execution only serves \
             a ClusterRepository, because the mover takes the repository's Secrets and volumes \
             from that namespace. Fix: point it back at the ClusterRepository, or delete it and \
             create a new {subject}"
        )),
    }
}

/// The namespace whose privileged-movers opt-in an elevated mover still lacks,
/// if any. The workload namespace must consent, as always. A central mover pod
/// runs in `mover_ns` instead, so that namespace must consent as well: one
/// namespace cannot opt another into privileged pods.
pub async fn privileged_opt_in_missing(
    client: &kube::Client,
    workload_ns: &str,
    mover_ns: &str,
) -> Result<Option<String>> {
    for ns in privileged_consent_namespaces(workload_ns, mover_ns) {
        if !super::mover::namespace_allows_privileged_movers(client, ns).await? {
            return Ok(Some(ns.to_string()));
        }
    }
    Ok(None)
}

/// The namespaces that must each opt in to an elevated mover: the workload's,
/// then the mover's when it is a different one.
pub fn privileged_consent_namespaces<'a>(workload_ns: &'a str, mover_ns: &'a str) -> Vec<&'a str> {
    if workload_ns == mover_ns {
        vec![workload_ns]
    } else {
        vec![workload_ns, mover_ns]
    }
}

/// Delete every central object of the `Restore` with UID `restore_uid`: its
/// mover Jobs (their pods cascade) and its prime PVCs. Called from the
/// `Restore`'s finalizer, because no owner reference reaches these objects.
/// Idempotent and 404-tolerant.
pub async fn reap_central_restore(
    client: &kube::Client,
    central_ns: &str,
    restore_uid: &str,
) -> Result<()> {
    let selector = format!("{CENTRAL_RESTORE_UID_LABEL}={restore_uid}");
    let params = kube::api::ListParams::default().labels(&selector);
    let jobs: Api<k8s_openapi::api::batch::v1::Job> = Api::namespaced(client.clone(), central_ns);
    for job in jobs.list(&params).await?.items {
        match jobs
            .delete(
                &kube::ResourceExt::name_any(&job),
                &delete_exactly(&job.metadata),
            )
            .await
        {
            Ok(_) => {}
            // Gone already, or replaced by another object of the same name.
            Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => {}
            Err(e) => return Err(Error::Kube(e)),
        }
    }
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), central_ns);
    for claim in claims.list(&params).await?.items {
        match claims
            .delete(
                &kube::ResourceExt::name_any(&claim),
                &delete_exactly(&claim.metadata),
            )
            .await
        {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => {}
            Err(e) => return Err(Error::Kube(e)),
        }
    }
    Ok(())
}

/// Delete parameters that remove exactly the object `meta` describes: its UID
/// is a precondition, so an object that replaced it under the same name is left
/// alone. Background propagation, so a Job's pods go with it.
pub fn delete_exactly(meta: &kube::core::ObjectMeta) -> kube::api::DeleteParams {
    kube::api::DeleteParams {
        propagation_policy: Some(kube::api::PropagationPolicy::Background),
        preconditions: Some(kube::api::Preconditions {
            uid: meta.uid.clone(),
            resource_version: None,
        }),
        ..Default::default()
    }
}

/// The finalizer the CSI external-provisioner (v6.3 and later) puts on a
/// `VolumeSnapshot` while a claim is being provisioned from it.
pub const PROVISIONER_SOURCE_PROTECTION_FINALIZER: &str =
    "provisioner.storage.kubernetes.io/volumesnapshot-as-source-protection";

/// Annotation Kopiur puts on a run's `VolumeSnapshot` once it has SEEN the run's
/// staged claim bound, i.e. the restore from that snapshot finished. It is the
/// evidence [`source_protection_decision`] requires, recorded on the object so
/// it outlives the claim.
pub const STAGE_RESTORED_ANNOTATION: &str = "kopiur.home-operations.com/stage-restored";

/// The most one release attempt may take. The attempt runs inside the terminal
/// and deletion paths of a Snapshot, and its API calls otherwise inherit the
/// client's long read timeout.
const RELEASE_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// How long a deleted `VolumeSnapshot` that cannot be released yet is polled
/// before cleanup stops waiting on it. Past this, whatever holds the snapshot is
/// not something Kopiur can settle, and the provisioner's own sweep is left to.
const SOURCE_PROTECTION_RETRY_WINDOW_SECS: i64 = 60;

/// Page size for the cluster-wide claim list in [`release_source_protection`].
const CLAIM_LIST_PAGE_SIZE: u32 = 500;

/// Whether the staged-source cleanup has anything left to come back for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageCleanup {
    /// Everything this run staged is deleted, or is being deleted with nothing
    /// left for Kopiur to do.
    Complete,
    /// The `VolumeSnapshot` is deleted but still held; look again shortly.
    Retry,
}

/// What to do about the provisioner's finalizer on a run's `VolumeSnapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceProtection {
    /// Nothing to release: the finalizer is absent, or the snapshot is not
    /// being deleted (a finalizer on a live snapshot is doing its job).
    NotHeld,
    /// Held, but Kopiur never saw the run's restore finish (the backup was
    /// aborted while the staged claim was still provisioning). The provisioner
    /// may still be creating a volume from the snapshot, so the finalizer is
    /// left to it.
    Unproven,
    /// The run's restore finished, but another claim is still being provisioned
    /// from the snapshot.
    Wait,
    /// The finalizer is stale: remove it so the deletion can finish.
    Release,
}

/// Decide [`SourceProtection`] for a `VolumeSnapshot`.
///
/// After a cross-namespace restore the external-provisioner does not remove its
/// finalizer (kubernetes-csi/external-provisioner#1613), so the snapshot Kopiur
/// deletes stays `Terminating` until the provisioner's orphan sweep runs, or for
/// good where that sweep is off.
///
/// Kopiur removes it only when all of these hold:
/// - Kopiur requested the deletion (`deleting`);
/// - Kopiur saw the run's own staged claim bound (`restored`). A claim that is
///   merely gone proves nothing: the provisioner keeps creating the volume of a
///   deleted claim, and that still reads the snapshot;
/// - no other claim anywhere is still being provisioned from the snapshot
///   (`pending_claims`), which is the provisioner sweep's own test.
pub fn source_protection_decision(
    finalizers: &[String],
    deleting: bool,
    restored: bool,
    pending_claims: bool,
) -> SourceProtection {
    let held = finalizers
        .iter()
        .any(|f| f == PROVISIONER_SOURCE_PROTECTION_FINALIZER);
    match (deleting && held, restored, pending_claims) {
        (false, _, _) => SourceProtection::NotHeld,
        (true, false, _) => SourceProtection::Unproven,
        (true, true, true) => SourceProtection::Wait,
        (true, true, false) => SourceProtection::Release,
    }
}

/// Whether a staged claim shows that its restore from the `VolumeSnapshot`
/// `vs_ns/vs_name` finished: it still names that snapshot as its data source
/// and it is bound to a volume. Only then is the provisioner done reading the
/// snapshot on this claim's behalf. A bound claim that lost its reference (the
/// feature gate is off) restored nothing and proves nothing.
pub fn stage_restore_finished(staged: &PersistentVolumeClaim, vs_ns: &str, vs_name: &str) -> bool {
    if !cross_namespace_ref_survived(staged, vs_ns, CrossNamespaceSource::VolumeSnapshot(vs_name)) {
        return false;
    }
    let bound_phase = staged
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .is_some_and(|phase| phase == "Bound");
    let has_volume = staged
        .spec
        .as_ref()
        .and_then(|s| s.volume_name.as_deref())
        .is_some_and(|v| !v.is_empty());
    bound_phase && has_volume
}

/// The `VolumeSnapshot` in `vs_ns` that a central staged claim is restored
/// from, read off the claim's cross-namespace `dataSourceRef`. `None` for a
/// clone (it names a PVC) and for a claim whose reference was dropped.
pub fn staged_from_snapshot(staged: &PersistentVolumeClaim, vs_ns: &str) -> Option<String> {
    let source = staged.spec.as_ref()?.data_source_ref.as_ref()?;
    (source.kind == "VolumeSnapshot" && source.namespace.as_deref() == Some(vs_ns))
        .then(|| source.name.clone())
}

/// Whether `pvc` is still being provisioned from the `VolumeSnapshot`
/// `vs_ns/vs_name`: it names that snapshot as its data source and has not bound.
/// The external-provisioner's own test for "this snapshot is still in use"
/// (`buildPendingSnapshotIndex` in its `snapshot_finalizer_controller.go`).
pub fn claim_pending_from_snapshot(
    pvc: &PersistentVolumeClaim,
    vs_ns: &str,
    vs_name: &str,
) -> bool {
    let pending = pvc
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .is_none_or(|phase| phase.is_empty() || phase == "Pending");
    if !pending {
        return false;
    }
    let Some(spec) = pvc.spec.as_ref() else {
        return false;
    };
    let is_snapshot = |kind: &str, group: Option<&str>| {
        kind == "VolumeSnapshot" && group.is_none_or(|g| g == SNAPSHOT_GROUP)
    };
    let claim_ns = pvc.metadata.namespace.as_deref().unwrap_or_default();
    // `dataSource` is namespace-local; `dataSourceRef` may name another namespace.
    if let Some(source) = &spec.data_source
        && is_snapshot(&source.kind, source.api_group.as_deref())
    {
        return claim_ns == vs_ns && source.name == vs_name;
    }
    if let Some(source) = &spec.data_source_ref
        && is_snapshot(&source.kind, source.api_group.as_deref())
    {
        let source_ns = source
            .namespace
            .as_deref()
            .filter(|ns| !ns.is_empty())
            .unwrap_or(claim_ns);
        return source_ns == vs_ns && source.name == vs_name;
    }
    false
}

/// Whether a snapshot whose deletion was requested at `deleted_at` (Unix
/// seconds) is still worth polling at `now`.
pub fn within_release_window(deleted_at: i64, now: i64) -> bool {
    now.saturating_sub(deleted_at) < SOURCE_PROTECTION_RETRY_WINDOW_SECS
}

/// Record on the run's `VolumeSnapshot` that its restore finished (see
/// [`STAGE_RESTORED_ANNOTATION`]). Best effort: a failure only means the
/// finalizer is left to the provisioner.
pub async fn mark_stage_restored(client: &kube::Client, ns: &str, vs_name: &str) {
    let patch = serde_json::json!({
        "metadata": { "annotations": { STAGE_RESTORED_ANNOTATION: "true" } }
    });
    let result = super::staging::volume_snapshot_api(client, ns)
        .patch(
            vs_name,
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(&patch),
        )
        .await;
    match result {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => tracing::debug!(
            namespace = ns,
            volume_snapshot = vs_name,
            error = %e,
            "could not record that the staged restore finished"
        ),
    }
}

/// Whether any claim in the cluster is still being provisioned from the
/// `VolumeSnapshot` `ns/vs_name`. Cluster-wide, like the provisioner's sweep: a
/// `ReferenceGrant` Kopiur did not write can let any namespace restore from it.
/// Paginated, because an incomplete list would read as "nobody is restoring".
async fn any_claim_pending_from(client: &kube::Client, ns: &str, vs_name: &str) -> Result<bool> {
    let claims: Api<PersistentVolumeClaim> = Api::all(client.clone());
    let mut token: Option<String> = None;
    loop {
        let mut params = kube::api::ListParams::default().limit(CLAIM_LIST_PAGE_SIZE);
        if let Some(t) = &token {
            params = params.continue_token(t);
        }
        let page = claims.list(&params).await?;
        if page
            .items
            .iter()
            .any(|pvc| claim_pending_from_snapshot(pvc, ns, vs_name))
        {
            return Ok(true);
        }
        token = page.metadata.continue_.filter(|t| !t.is_empty());
        if token.is_none() {
            return Ok(false);
        }
    }
}

/// What one attempt at the release found.
enum ReleaseAttempt {
    /// Nothing more to do: gone, not held, unproven, or released.
    Settled,
    /// Not releasable right now; `deleted_at` (Unix seconds) bounds the wait.
    Blocked { deleted_at: i64 },
}

/// One attempt to release `ns/vs_name` (see [`release_source_protection`]).
async fn attempt_release(client: &kube::Client, ns: &str, vs_name: &str) -> Result<ReleaseAttempt> {
    let vs_api = super::staging::volume_snapshot_api(client, ns);
    let Some(vs) = vs_api.get_opt(vs_name).await? else {
        return Ok(ReleaseAttempt::Settled);
    };
    let finalizers = vs.metadata.finalizers.clone().unwrap_or_default();
    let deleted_at = vs
        .metadata
        .deletion_timestamp
        .as_ref()
        .map(|t| t.0.as_second());
    let restored = vs.metadata.annotations.as_ref().is_some_and(|a| {
        a.get(STAGE_RESTORED_ANNOTATION)
            .is_some_and(|v| v == "true")
    });
    // The claim list is cluster-wide; skip it unless it can change the answer.
    match source_protection_decision(&finalizers, deleted_at.is_some(), restored, false) {
        SourceProtection::NotHeld | SourceProtection::Unproven => {
            return Ok(ReleaseAttempt::Settled);
        }
        SourceProtection::Wait | SourceProtection::Release => {}
    }
    let blocked = ReleaseAttempt::Blocked {
        deleted_at: deleted_at.unwrap_or_default(),
    };
    if any_claim_pending_from(client, ns, vs_name).await? {
        return Ok(blocked);
    }
    let remaining: Vec<&String> = finalizers
        .iter()
        .filter(|f| *f != PROVISIONER_SOURCE_PROTECTION_FINALIZER)
        .collect();
    // The resourceVersion makes this a compare-and-swap: a finalizer list that
    // changed since the read is re-evaluated, not overwritten.
    let patch = serde_json::json!({
        "metadata": {
            "resourceVersion": vs.metadata.resource_version,
            "finalizers": remaining,
        }
    });
    match vs_api
        .patch(
            vs_name,
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(&patch),
        )
        .await
    {
        Ok(_) => {
            tracing::info!(
                namespace = ns,
                volume_snapshot = vs_name,
                "removed the CSI provisioner's stale source-protection finalizer"
            );
            Ok(ReleaseAttempt::Settled)
        }
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(ReleaseAttempt::Settled),
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(blocked),
        Err(e) => Err(Error::Kube(e)),
    }
}

/// Release a central run's deleted `VolumeSnapshot` from the provisioner's stale
/// finalizer (see [`source_protection_decision`]).
///
/// This is an optional workaround, so it never fails the reconcile and never
/// holds it for long: one attempt is cut off at [`RELEASE_ATTEMPT_TIMEOUT`], an
/// API error or a cut-off is logged and read as "not released", and
/// [`StageCleanup::Retry`] is returned only inside the bounded window after the
/// deletion. Past it the finalizer is left to the provisioner; the next
/// terminal reconcile of the Snapshot tries again.
pub async fn release_source_protection(
    client: &kube::Client,
    ns: &str,
    vs_name: &str,
) -> StageCleanup {
    let attempt = tokio::time::timeout(
        RELEASE_ATTEMPT_TIMEOUT,
        attempt_release(client, ns, vs_name),
    )
    .await;
    let attempt = match attempt {
        Ok(attempt) => attempt,
        Err(_) => {
            tracing::warn!(
                namespace = ns,
                volume_snapshot = vs_name,
                "timed out checking the CSI provisioner's source-protection finalizer; leaving \
                 it to the provisioner"
            );
            return StageCleanup::Complete;
        }
    };
    match attempt {
        Ok(ReleaseAttempt::Settled) => StageCleanup::Complete,
        Ok(ReleaseAttempt::Blocked { deleted_at })
            if within_release_window(deleted_at, chrono::Utc::now().timestamp()) =>
        {
            StageCleanup::Retry
        }
        Ok(ReleaseAttempt::Blocked { .. }) => StageCleanup::Complete,
        Err(e) => {
            tracing::warn!(
                namespace = ns,
                volume_snapshot = vs_name,
                error = %e,
                "could not check the CSI provisioner's source-protection finalizer; leaving it \
                 to the provisioner"
            );
            StageCleanup::Complete
        }
    }
}

/// Whether the Job `job_name` in `namespace` is a central mover Job, which an
/// in-namespace run of that name must not delete. Central Jobs exist only in the
/// operator namespace, so every other namespace answers `false` without a read.
pub async fn is_another_runs_central_job(
    client: &kube::Client,
    operator_namespace: Option<&str>,
    namespace: &str,
    job_name: &str,
) -> Result<bool> {
    if operator_namespace != Some(namespace) {
        return Ok(false);
    }
    let jobs: Api<k8s_openapi::api::batch::v1::Job> = Api::namespaced(client.clone(), namespace);
    Ok(jobs
        .get_opt(job_name)
        .await?
        .is_some_and(|job| MoverSite::InNamespace.is_foreign(&job.metadata, "")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{PersistentVolumeClaimSpec, TypedLocalObjectReference};
    use kube::core::ObjectMeta;

    fn owner(uid: &str) -> OwnerReference {
        OwnerReference {
            api_version: "kopiur.home-operations.com/v1alpha1".into(),
            kind: "Snapshot".into(),
            name: "nightly".into(),
            uid: uid.into(),
            controller: Some(true),
            block_owner_deletion: Some(false),
        }
    }

    fn in_namespace_staged_pvc() -> PersistentVolumeClaim {
        PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some("nightly-src".into()),
                namespace: Some("kopiur-system".into()),
                owner_references: Some(vec![owner("uid-1")]),
                labels: Some(child_labels(&[])),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                data_source: Some(TypedLocalObjectReference {
                    api_group: Some(SNAPSHOT_GROUP.into()),
                    kind: "VolumeSnapshot".into(),
                    name: "nightly-snap".into(),
                }),
                ..Default::default()
            }),
            status: None,
        }
    }

    #[test]
    fn central_job_names_differ_across_namespaces_and_fit_a_job_name() {
        let a = central_job_name("billing", "nightly");
        let b = central_job_name("payments", "nightly");
        assert_ne!(
            a, b,
            "same Snapshot name in two namespaces must not collide"
        );
        assert!(a.starts_with("nightly-"), "{a}");
        assert_eq!(a, central_job_name("billing", "nightly"), "must be stable");
        // The namespace/name split is part of the hash input, not just the
        // concatenation: `a-b` + `c` and `a` + `b-c` are different Snapshots.
        assert_ne!(central_job_name("a-b", "c"), central_job_name("a", "b-c"));
        let long = central_job_name("ns", &"x".repeat(200));
        assert!(long.len() <= kopiur_api::consts::JOB_NAME_MAX, "{long}");
        // A truncation that lands on a separator must not leave `--`.
        let dashed = central_job_name("ns", &format!("{}-{}", "y".repeat(39), "z".repeat(20)));
        assert!(!dashed.contains("--"), "{dashed}");
    }

    #[test]
    fn pinned_site_comes_from_status_job_namespace_only() {
        assert_eq!(
            MoverSite::pinned(None, "billing", "nightly"),
            MoverSite::InNamespace
        );
        // A name without a namespace is not a pin (an in-namespace Job).
        let named = JobStatus {
            name: Some("nightly".into()),
            ..Default::default()
        };
        assert_eq!(
            MoverSite::pinned(Some(&named), "billing", "nightly"),
            MoverSite::InNamespace
        );
        let central = JobStatus {
            name: Some("nightly-abc".into()),
            namespace: Some("kopiur-system".into()),
            ..Default::default()
        };
        let site = MoverSite::pinned(Some(&central), "billing", "nightly");
        assert_eq!(site.namespace("billing"), "kopiur-system");
        assert_eq!(site.job_name("nightly"), "nightly-abc");
        assert_eq!(site.staged_pvc_name("nightly"), "nightly-abc-src");
        // The pin round-trips through what `status_pin` writes.
        let pin: JobStatus = serde_json::from_value(site.status_pin().unwrap()).unwrap();
        assert_eq!(MoverSite::pinned(Some(&pin), "billing", "nightly"), site);
    }

    #[test]
    fn in_namespace_site_keeps_the_historical_names_and_writes_no_pin() {
        let site = MoverSite::InNamespace;
        assert_eq!(site.namespace("billing"), "billing");
        assert_eq!(site.job_name("nightly"), "nightly");
        assert_eq!(site.staged_pvc_name("nightly"), "nightly-src");
        assert_eq!(site.status_pin(), None);
    }

    #[test]
    fn central_is_chosen_only_for_an_unstaged_run_or_a_pinned_one() {
        let pinned = MoverSite::Central(CentralSite {
            namespace: "kopiur-system".into(),
            job_name: "j".into(),
        });
        use ExecutionMode::{Central, InNamespace};
        assert!(wants_central(&MoverSite::InNamespace, Central, false));
        // Already staged beside the source: the repository was switched mid-run.
        assert!(!wants_central(&MoverSite::InNamespace, Central, true));
        assert!(!wants_central(&MoverSite::InNamespace, InNamespace, false));
        // A pinned run stays central even after the repository drops `execution`.
        assert!(wants_central(&pinned, InNamespace, false));
        assert!(wants_central(&pinned, Central, true));
    }

    #[test]
    fn a_source_runs_centrally_only_when_it_can_leave_the_workload_namespace() {
        use CentralSource::{GroupMember, Nfs, Nothing, Pvc, Stream};
        // A PVC crosses as a CSI snapshot or a CSI clone, alone or as a member
        // of a consistency group.
        for (method, source) in [
            (CopyMethod::Snapshot, Pvc),
            (CopyMethod::Clone, Pvc),
            (CopyMethod::Snapshot, GroupMember),
            (CopyMethod::Clone, GroupMember),
        ] {
            assert_eq!(
                central_source_refusal(method, source, "shared"),
                None,
                "{method:?}/{source:?} must run centrally"
            );
        }
        for (method, source, needle) in [
            (CopyMethod::Direct, Pvc, "copyMethod: Direct"),
            (CopyMethod::Direct, GroupMember, "copyMethod: Direct"),
            // A central pod would mount the export outside the reach of the
            // workload namespace's Pod Security rules, under every copy method.
            (CopyMethod::Snapshot, Nfs, "Pod Security"),
            (CopyMethod::Clone, Nfs, "Pod Security"),
            (CopyMethod::Direct, Nfs, "Pod Security"),
            (CopyMethod::Snapshot, Stream, "stream"),
            // The source shape wins over the copy method in the explanation.
            (CopyMethod::Direct, Stream, "stream"),
            (CopyMethod::Snapshot, Nothing, "no PVC and no NFS export"),
        ] {
            let (reason, message) = central_source_refusal(method, source, "shared")
                .unwrap_or_else(|| panic!("{method:?}/{source:?} must be refused"));
            assert_eq!(reason, REASON_CENTRAL_UNSUPPORTED);
            assert!(message.contains(needle), "{message}");
            assert!(message.contains("ClusterRepository `shared`"), "{message}");
            assert!(message.contains("Fix:"), "{message}");
        }
    }

    #[test]
    fn central_site_needs_a_cluster_install_and_a_known_operator_namespace() {
        let site = central_site_for(Some("kopiur-system"), false, "billing", "nightly").unwrap();
        assert_eq!(site.namespace, "kopiur-system");
        assert_eq!(site.job_name, central_job_name("billing", "nightly"));

        let (reason, message) =
            central_site_for(Some("kopiur-system"), true, "billing", "nightly").unwrap_err();
        assert_eq!(reason, REASON_CENTRAL_UNAVAILABLE);
        assert!(message.contains("namespaced install"), "{message}");

        for unset in [None, Some("")] {
            let (reason, message) =
                central_site_for(unset, false, "billing", "nightly").unwrap_err();
            assert_eq!(reason, REASON_CENTRAL_UNAVAILABLE);
            assert!(message.contains("KOPIUR_NAMESPACE"), "{message}");
        }
    }

    #[test]
    fn centralizing_a_staged_pvc_swaps_the_data_source_and_the_ownership() {
        let mut pvc = in_namespace_staged_pvc();
        let snap = CrossNamespaceSource::VolumeSnapshot("nightly-snap");
        centralize_staged_pvc(&mut pvc, "billing", snap, "nightly", "uid-1");

        let spec = pvc.spec.as_ref().unwrap();
        // The API server rejects `dataSource` beside `dataSourceRef.namespace`.
        assert_eq!(spec.data_source, None);
        let r = spec.data_source_ref.as_ref().unwrap();
        assert_eq!(r.api_group.as_deref(), Some(SNAPSHOT_GROUP));
        assert_eq!(r.kind, "VolumeSnapshot");
        assert_eq!(r.name, "nightly-snap");
        assert_eq!(r.namespace.as_deref(), Some("billing"));

        // A cross-namespace owner reference gets the object garbage collected.
        assert_eq!(pvc.metadata.owner_references, None);
        assert!(is_ours(&pvc.metadata, "uid-1"));
        assert!(!is_ours(&pvc.metadata, "uid-2"));
        assert_eq!(
            pvc.metadata.annotations.as_ref().unwrap()[CENTRAL_SNAPSHOT_ANNOTATION],
            "billing/nightly"
        );
        // The managed-by label the builder set survives.
        assert!(
            pvc.metadata
                .labels
                .as_ref()
                .unwrap()
                .contains_key(crate::consts::MANAGED_BY_LABEL)
        );
        assert!(cross_namespace_ref_survived(&pvc, "billing", snap));
    }

    /// `copyMethod: Clone` names the source PVC itself. The core group is an
    /// ABSENT `apiGroup` on the claim and an empty `group` on the grant: that
    /// pair is what the CSI provisioner's grant check matches.
    #[test]
    fn a_clone_stage_references_the_source_pvc_across_namespaces() {
        let clone = CrossNamespaceSource::Pvc("data");
        let mut pvc = in_namespace_staged_pvc();
        centralize_staged_pvc(&mut pvc, "billing", clone, "nightly", "uid-1");
        let spec = pvc.spec.as_ref().unwrap();
        assert_eq!(spec.data_source, None);
        let r = spec.data_source_ref.as_ref().unwrap();
        assert_eq!(r.api_group, None);
        assert_eq!(r.kind, "PersistentVolumeClaim");
        assert_eq!(r.name, "data");
        assert_eq!(r.namespace.as_deref(), Some("billing"));
        assert!(cross_namespace_ref_survived(&pvc, "billing", clone));
        // A snapshot of the same name is a different object.
        assert!(!cross_namespace_ref_survived(
            &pvc,
            "billing",
            CrossNamespaceSource::VolumeSnapshot("data")
        ));

        let grant = build_reference_grant(
            "nightly-grant",
            "billing",
            "kopiur-system",
            clone,
            owner("uid-1"),
        );
        assert_eq!(
            grant.data["spec"]["to"],
            serde_json::json!([{
                "group": "",
                "kind": "PersistentVolumeClaim",
                "name": "data",
            }])
        );
    }

    /// A group member restores from a member snapshot, whose name only its
    /// staged claim records. Cleanup reads it from there.
    #[test]
    fn the_snapshot_a_central_stage_restores_from_is_read_off_the_claim() {
        let mut from_snapshot = in_namespace_staged_pvc();
        centralize_staged_pvc(
            &mut from_snapshot,
            "billing",
            CrossNamespaceSource::VolumeSnapshot("snapshot-4f2a"),
            "nightly",
            "uid-1",
        );
        assert_eq!(
            staged_from_snapshot(&from_snapshot, "billing").as_deref(),
            Some("snapshot-4f2a")
        );
        // Another namespace's snapshot of that name is not it.
        assert_eq!(staged_from_snapshot(&from_snapshot, "payments"), None);

        // A clone names a PVC: there is no snapshot to mark or release.
        let mut clone = in_namespace_staged_pvc();
        centralize_staged_pvc(
            &mut clone,
            "billing",
            CrossNamespaceSource::Pvc("data"),
            "nightly",
            "uid-1",
        );
        assert_eq!(staged_from_snapshot(&clone, "billing"), None);
        assert_eq!(
            staged_from_snapshot(&PersistentVolumeClaim::default(), "billing"),
            None
        );
    }

    /// The data-loss guard: with the feature gate off, kube-apiserver stores the
    /// claim WITHOUT its `dataSourceRef`, i.e. as a request for an empty volume.
    #[test]
    fn a_stored_claim_without_the_cross_namespace_ref_is_detected() {
        let snap = CrossNamespaceSource::VolumeSnapshot("nightly-snap");
        let mut pvc = in_namespace_staged_pvc();
        centralize_staged_pvc(&mut pvc, "billing", snap, "nightly", "uid-1");

        let mut dropped = pvc.clone();
        dropped.spec.as_mut().unwrap().data_source_ref = None;
        assert!(!cross_namespace_ref_survived(&dropped, "billing", snap));

        // Namespace stripped: it would resolve to a snapshot in the operator namespace.
        let mut local = pvc.clone();
        local
            .spec
            .as_mut()
            .unwrap()
            .data_source_ref
            .as_mut()
            .unwrap()
            .namespace = None;
        assert!(!cross_namespace_ref_survived(&local, "billing", snap));

        assert!(!cross_namespace_ref_survived(&pvc, "payments", snap));
        assert!(!cross_namespace_ref_survived(
            &pvc,
            "billing",
            CrossNamespaceSource::VolumeSnapshot("other-snap")
        ));
        let no_spec = PersistentVolumeClaim::default();
        assert!(!cross_namespace_ref_survived(&no_spec, "billing", snap));

        let message = cross_namespace_disabled_message("kopiur-system", "nightly-abc-src");
        assert!(
            message.contains("CrossNamespaceVolumeDataSource=true"),
            "{message}"
        );
        assert!(message.contains("no backup was taken"), "{message}");
    }

    #[test]
    fn the_reference_grant_names_one_snapshot_and_one_consumer_namespace() {
        let grant = build_reference_grant(
            "nightly-grant",
            "billing",
            "kopiur-system",
            CrossNamespaceSource::VolumeSnapshot("nightly-snap"),
            owner("uid-1"),
        );
        assert_eq!(grant.metadata.namespace.as_deref(), Some("billing"));
        assert_eq!(
            grant.types.as_ref().unwrap().api_version,
            "gateway.networking.k8s.io/v1beta1"
        );
        assert_eq!(
            grant.data["spec"],
            serde_json::json!({
                "from": [{
                    "group": "",
                    "kind": "PersistentVolumeClaim",
                    "namespace": "kopiur-system",
                }],
                // `name` narrows the grant to this run's snapshot; without it
                // every VolumeSnapshot in the namespace would be restorable.
                "to": [{
                    "group": "snapshot.storage.k8s.io",
                    "kind": "VolumeSnapshot",
                    "name": "nightly-snap",
                }],
            })
        );
        // Same namespace as the Snapshot CR, so the owner reference is valid.
        assert_eq!(grant.metadata.owner_references, Some(vec![owner("uid-1")]));
        assert_eq!(reference_grant_name("nightly"), "nightly-grant");
    }

    #[test]
    fn the_status_binding_lives_with_the_snapshot_and_names_the_central_service_account() {
        let rb = build_central_status_binding(
            "billing",
            "kopiur-central-mover",
            "kopiur-central-mover",
            "kopiur-system",
            "kopiur-central-mover",
        );
        assert_eq!(rb.metadata.namespace.as_deref(), Some("billing"));
        assert_eq!(rb.metadata.name.as_deref(), Some("kopiur-central-mover"));
        assert_eq!(rb.role_ref.kind, "ClusterRole");
        assert_eq!(rb.role_ref.name, "kopiur-central-mover");
        let subject = &rb.subjects.as_ref().unwrap()[0];
        assert_eq!(subject.kind, "ServiceAccount");
        assert_eq!(subject.name, "kopiur-central-mover");
        assert_eq!(subject.namespace.as_deref(), Some("kopiur-system"));
    }

    #[test]
    fn only_a_central_bind_failure_carries_the_provisioner_hint() {
        let central = MoverSite::Central(CentralSite {
            namespace: "kopiur-system".into(),
            job_name: "j".into(),
        });
        let hinted = central.with_bind_hint("did not bind".into());
        assert!(hinted.starts_with("did not bind. "), "{hinted}");
        assert!(hinted.contains("external-provisioner"), "{hinted}");
        assert!(hinted.contains("referencegrants"), "{hinted}");
        assert_eq!(
            MoverSite::InNamespace.with_bind_hint("did not bind".into()),
            "did not bind"
        );
    }

    #[test]
    fn a_central_object_is_foreign_to_every_run_but_its_own() {
        let central = MoverSite::Central(CentralSite {
            namespace: "kopiur-system".into(),
            job_name: "j".into(),
        });
        let mut marked = ObjectMeta::default();
        mark_central(&mut marked, "billing", "nightly", "uid-1");
        let plain = ObjectMeta::default();

        // Its own run.
        assert!(!central.is_foreign(&marked, "uid-1"));
        // Another central run, and an in-namespace run whose Snapshot happens to
        // be named like the central object: neither may touch it.
        assert!(central.is_foreign(&marked, "uid-2"));
        assert!(MoverSite::InNamespace.is_foreign(&marked, "uid-1"));
        // An unlabelled object is an in-namespace run's, never a central run's.
        assert!(!MoverSite::InNamespace.is_foreign(&plain, "uid-1"));
        assert!(central.is_foreign(&plain, "uid-1"));
    }

    #[test]
    fn a_central_run_always_has_a_stage_to_reap() {
        let central = MoverSite::Central(CentralSite {
            namespace: "kopiur-system".into(),
            job_name: "j".into(),
        });
        // The staged PVC exists before `status.staged` is stamped.
        assert!(central.may_have_stage(false));
        assert!(central.may_have_stage(true));
        assert!(!MoverSite::InNamespace.may_have_stage(false));
        assert!(MoverSite::InNamespace.may_have_stage(true));
    }

    #[test]
    fn a_repository_path_that_overlaps_the_central_source_mount_is_refused() {
        // Same directory, either one inside the other, trailing slashes.
        for overlapping in [
            "/source",
            "/source/",
            "/source/repo",
            "/",
            "//source//repo",
            // Spelled around the comparison: still the same directory.
            "/./source/repo",
            "/source/./repo",
            // Not resolved, so refused.
            "/repo/../source",
            "/data/..",
        ] {
            assert!(
                mount_paths_overlap(overlapping, CENTRAL_SOURCE_MOUNT_PATH),
                "{overlapping}"
            );
            let (reason, message) =
                central_repo_mount_refusal(Some(overlapping), "shared").unwrap();
            assert_eq!(reason, REASON_CENTRAL_UNSUPPORTED);
            assert!(message.contains("Fix:"), "{message}");
        }
        // A shared name prefix is not a shared directory.
        for apart in [
            "/sources",
            "/repo",
            "/data/source",
            "/source-repo",
            "/./repo",
        ] {
            assert!(
                !mount_paths_overlap(apart, CENTRAL_SOURCE_MOUNT_PATH),
                "{apart}"
            );
            assert_eq!(central_repo_mount_refusal(Some(apart), "shared"), None);
        }
        // An object-store repository mounts nothing.
        assert_eq!(central_repo_mount_refusal(None, "shared"), None);
    }

    #[test]
    fn a_source_is_classified_by_what_it_mounts() {
        use kopiur_api::expand::EffectiveSource;
        let source = |pvc: bool, nfs: bool, stream: bool| EffectiveSource {
            index: 0,
            pvc: pvc.then(|| kopiur_api::PvcTargetRef {
                namespace: "billing".into(),
                name: "data".into(),
            }),
            nfs_path: nfs.then(|| "/exports/data".to_string()),
            stream: stream.then(|| kopiur_api::snapshot_policy::StreamSource {
                file_name: "dump.sql".into(),
                workload_exec: kopiur_api::StreamExec {
                    pod_selector: Default::default(),
                    container: None,
                    command: vec!["pg_dump".into()],
                    timeout: None,
                },
            }),
            // An override changes the path kopia records, never the mount path
            // of a central run, so it does not change the class.
            source_path_override: Some("/usr/local/bin".into()),
            read_only: true,
        };
        assert_eq!(
            CentralSource::of(&source(true, false, false), false),
            CentralSource::Pvc
        );
        assert_eq!(
            CentralSource::of(&source(true, false, false), true),
            CentralSource::GroupMember
        );
        assert_eq!(
            CentralSource::of(&source(false, true, false), false),
            CentralSource::Nfs
        );
        assert_eq!(
            CentralSource::of(&source(false, false, true), false),
            CentralSource::Stream
        );
        assert_eq!(
            CentralSource::of(&source(false, false, false), false),
            CentralSource::Nothing
        );
    }

    fn finalizers(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// kubernetes-csi/external-provisioner#1613: the provisioner's finalizer is
    /// left on the VolumeSnapshot after a cross-namespace restore. It is removed
    /// only when Kopiur asked for the deletion, saw the restore finish, and
    /// nothing else is still restoring.
    #[test]
    fn the_stale_provisioner_finalizer_is_released_only_with_proof_the_restore_finished() {
        use SourceProtection::{NotHeld, Release, Unproven, Wait};
        let held = finalizers(&[
            "snapshot.storage.kubernetes.io/volumesnapshot-bound-protection",
            PROVISIONER_SOURCE_PROTECTION_FINALIZER,
        ]);
        assert_eq!(
            source_protection_decision(&held, true, true, false),
            Release
        );
        // Another claim is still being provisioned from it.
        assert_eq!(source_protection_decision(&held, true, true, true), Wait);
        // The run was aborted while its claim was provisioning. The claim being
        // gone proves nothing: the provisioner still creates a deleted claim's
        // volume, and that reads the snapshot. Never release on absence alone.
        assert_eq!(
            source_protection_decision(&held, true, false, false),
            Unproven
        );
        assert_eq!(
            source_protection_decision(&held, true, false, true),
            Unproven
        );
        // Not being deleted: never strip a finalizer from a live snapshot.
        assert_eq!(
            source_protection_decision(&held, false, true, false),
            NotHeld
        );
        // Held by someone else's finalizer only: not Kopiur's to release.
        let others =
            finalizers(&["snapshot.storage.kubernetes.io/volumesnapshot-as-source-protection"]);
        assert_eq!(
            source_protection_decision(&others, true, true, false),
            NotHeld
        );
        assert_eq!(source_protection_decision(&[], true, true, false), NotHeld);
    }

    #[test]
    fn a_restore_counts_as_finished_only_for_a_bound_claim_of_that_snapshot() {
        let pvc = |phase: Option<&str>, volume: Option<&str>| -> PersistentVolumeClaim {
            serde_json::from_value(serde_json::json!({
                "metadata": { "name": "c", "namespace": "kopiur-system" },
                "spec": {
                    "volumeName": volume,
                    "dataSourceRef": {
                        "apiGroup": SNAPSHOT_GROUP, "kind": "VolumeSnapshot",
                        "name": "snap", "namespace": "billing",
                    },
                },
                "status": { "phase": phase },
            }))
            .unwrap()
        };
        let finished = |pvc: &PersistentVolumeClaim| stage_restore_finished(pvc, "billing", "snap");
        assert!(finished(&pvc(Some("Bound"), Some("pvc-1"))));
        // Still provisioning, in every spelling of it.
        assert!(!finished(&pvc(Some("Pending"), None)));
        assert!(!finished(&pvc(None, None)));
        assert!(!finished(&pvc(Some("Pending"), Some("pvc-1"))));
        assert!(!finished(&pvc(Some("Bound"), None)));
        assert!(!finished(&pvc(Some("Bound"), Some(""))));
        assert!(!finished(&pvc(Some("Lost"), Some("pvc-1"))));
        assert!(!finished(&PersistentVolumeClaim::default()));

        // Bound, but not from this snapshot: the feature gate was off and the
        // API server dropped the reference, so the claim is an empty volume.
        let mut unreferenced = pvc(Some("Bound"), Some("pvc-1"));
        unreferenced.spec.as_mut().unwrap().data_source_ref = None;
        assert!(!finished(&unreferenced));
        let bound = pvc(Some("Bound"), Some("pvc-1"));
        assert!(!stage_restore_finished(&bound, "billing", "other"));
        assert!(!stage_restore_finished(&bound, "payments", "snap"));
    }

    fn claim(
        ns: &str,
        phase: Option<&str>,
        data_source: Option<&str>,
        data_source_ref: Option<(&str, Option<&str>)>,
    ) -> PersistentVolumeClaim {
        serde_json::from_value(serde_json::json!({
            "metadata": { "name": "c", "namespace": ns },
            "spec": {
                "dataSource": data_source.map(|name| serde_json::json!({
                    "apiGroup": SNAPSHOT_GROUP, "kind": "VolumeSnapshot", "name": name,
                })),
                "dataSourceRef": data_source_ref.map(|(name, namespace)| serde_json::json!({
                    "apiGroup": SNAPSHOT_GROUP, "kind": "VolumeSnapshot",
                    "name": name, "namespace": namespace,
                })),
            },
            "status": { "phase": phase },
        }))
        .unwrap()
    }

    /// The external-provisioner's own "still in use" test: a claim that names
    /// the snapshot and has not bound.
    #[test]
    fn only_an_unbound_claim_of_that_snapshot_holds_it() {
        let cross = |phase| {
            claim(
                "kopiur-system",
                phase,
                None,
                Some(("snap", Some("billing"))),
            )
        };
        assert!(claim_pending_from_snapshot(
            &cross(Some("Pending")),
            "billing",
            "snap"
        ));
        // No status yet is as unbound as `Pending`.
        assert!(claim_pending_from_snapshot(&cross(None), "billing", "snap"));
        assert!(claim_pending_from_snapshot(
            &cross(Some("")),
            "billing",
            "snap"
        ));
        // Provisioning finished: the snapshot is no longer needed.
        assert!(!claim_pending_from_snapshot(
            &cross(Some("Bound")),
            "billing",
            "snap"
        ));
        assert!(!claim_pending_from_snapshot(
            &cross(Some("Lost")),
            "billing",
            "snap"
        ));

        // A namespace-local `dataSource`, and a `dataSourceRef` without a namespace.
        let local = claim("billing", Some("Pending"), Some("snap"), None);
        assert!(claim_pending_from_snapshot(&local, "billing", "snap"));
        let local_ref = claim("billing", Some("Pending"), None, Some(("snap", None)));
        assert!(claim_pending_from_snapshot(&local_ref, "billing", "snap"));
        let empty_ns = claim("billing", Some("Pending"), None, Some(("snap", Some(""))));
        assert!(claim_pending_from_snapshot(&empty_ns, "billing", "snap"));

        // Another snapshot, or the same name in another namespace.
        assert!(!claim_pending_from_snapshot(&local, "billing", "other"));
        assert!(!claim_pending_from_snapshot(&local, "payments", "snap"));
        let elsewhere = claim(
            "kopiur-system",
            Some("Pending"),
            None,
            Some(("snap", Some("payments"))),
        );
        assert!(!claim_pending_from_snapshot(&elsewhere, "billing", "snap"));

        // Not a snapshot restore at all.
        let plain = claim("billing", Some("Pending"), None, None);
        assert!(!claim_pending_from_snapshot(&plain, "billing", "snap"));
        let clone: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
            "metadata": { "name": "c", "namespace": "billing" },
            "spec": { "dataSource": { "kind": "PersistentVolumeClaim", "name": "snap" } },
            "status": { "phase": "Pending" },
        }))
        .unwrap();
        assert!(!claim_pending_from_snapshot(&clone, "billing", "snap"));
    }

    /// The wait on a held snapshot is bounded, so a claim Kopiur does not own
    /// cannot keep a terminal Snapshot polling (or its deletion waiting) forever.
    #[test]
    fn a_held_snapshot_is_polled_for_a_bounded_time() {
        assert!(within_release_window(1_000, 1_000));
        assert!(within_release_window(1_000, 1_059));
        assert!(!within_release_window(1_000, 1_060));
        assert!(!within_release_window(1_000, 5_000));
        // A deletion timestamp ahead of this clock still counts as recent.
        assert!(within_release_window(1_000, 990));
    }

    fn restore_status(pin: Option<&str>) -> kopiur_api::RestoreStatus {
        kopiur_api::RestoreStatus {
            execution: pin.map(|ns| kopiur_api::RestoreExecutionStatus {
                namespace: ns.to_string(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_restore_site_is_central_only_when_its_status_pins_a_namespace() {
        assert_eq!(RestoreSite::pinned(None), RestoreSite::InNamespace);
        assert_eq!(
            RestoreSite::pinned(Some(&restore_status(None))),
            RestoreSite::InNamespace
        );
        let central = RestoreSite::pinned(Some(&restore_status(Some("kopiur-system"))));
        assert_eq!(
            central,
            RestoreSite::Central {
                namespace: "kopiur-system".into()
            }
        );
        // The Restore's own namespace is where its claims are; the pin is where
        // the prime PVCs and mover Jobs are.
        assert_eq!(central.namespace("billing"), "kopiur-system");
        assert_eq!(RestoreSite::InNamespace.namespace("billing"), "billing");
    }

    #[test]
    fn a_populator_restore_goes_central_only_before_it_creates_anything_in_namespace() {
        use ExecutionMode::{Central, InNamespace};
        let pinned = RestoreSite::Central {
            namespace: "kopiur-system".into(),
        };
        assert!(restore_wants_central(
            &RestoreSite::InNamespace,
            Central,
            false
        ));
        // A prime PVC or mover Job already stands beside the claims: the
        // repository was switched afterwards, and moving would strand them.
        assert!(!restore_wants_central(
            &RestoreSite::InNamespace,
            Central,
            true
        ));
        assert!(!restore_wants_central(
            &RestoreSite::InNamespace,
            InNamespace,
            false
        ));
        // A pinned restore stays central after the repository drops `execution`.
        assert!(restore_wants_central(&pinned, InNamespace, false));
        assert!(restore_wants_central(&pinned, Central, true));
    }

    /// Backup and restore movers of every namespace share the operator
    /// namespace. Each run owns only what carries ITS mark, and an in-namespace
    /// run owns nothing that carries any central mark.
    #[test]
    fn central_restore_objects_are_foreign_to_every_other_run() {
        let central = RestoreSite::Central {
            namespace: "kopiur-system".into(),
        };
        let mut restore_obj = ObjectMeta {
            owner_references: Some(vec![owner("restore-uid-1")]),
            ..Default::default()
        };
        mark_central_restore(&mut restore_obj, "billing", "restore-pg", "restore-uid-1");
        // A cross-namespace owner reference would get it garbage collected.
        assert_eq!(restore_obj.owner_references, None);
        assert_eq!(
            restore_obj.annotations.as_ref().unwrap()[CENTRAL_RESTORE_ANNOTATION],
            "billing/restore-pg"
        );
        assert!(has_central_mark(&restore_obj));

        assert!(!central.is_foreign(&restore_obj, "restore-uid-1"));
        assert!(central.is_foreign(&restore_obj, "restore-uid-2"));
        // An in-namespace restore, and an in-namespace or central BACKUP, that
        // happen to derive the same name.
        assert!(RestoreSite::InNamespace.is_foreign(&restore_obj, "restore-uid-1"));
        assert!(MoverSite::InNamespace.is_foreign(&restore_obj, "restore-uid-1"));
        let backup_site = MoverSite::Central(CentralSite {
            namespace: "kopiur-system".into(),
            job_name: "j".into(),
        });
        assert!(backup_site.is_foreign(&restore_obj, "restore-uid-1"));

        // And the reverse: a central backup's object is not a restore's.
        let mut backup_obj = ObjectMeta::default();
        mark_central(&mut backup_obj, "billing", "nightly", "snap-uid-1");
        assert!(central.is_foreign(&backup_obj, "snap-uid-1"));
        assert!(RestoreSite::InNamespace.is_foreign(&backup_obj, "snap-uid-1"));

        // An unmarked object belongs to an in-namespace run only.
        let plain = ObjectMeta::default();
        assert!(!RestoreSite::InNamespace.is_foreign(&plain, "restore-uid-1"));
        assert!(central.is_foreign(&plain, "restore-uid-1"));
    }

    /// A central mover takes Secrets and volumes from the operator namespace by
    /// name. A tenant-authored namespaced Repository must never supply those
    /// names, also after the run was pinned central against a ClusterRepository.
    #[test]
    fn a_central_run_refuses_a_namespaced_repository() {
        use kopiur_api::common::RepositoryKind;
        let central = RestoreSite::Central {
            namespace: "kopiur-system".into(),
        };
        assert_eq!(
            central_repository_refusal(
                &central,
                RepositoryKind::ClusterRepository,
                "Restore",
                "restore-pg"
            ),
            None
        );
        let msg = central_repository_refusal(
            &central,
            RepositoryKind::Repository,
            "Restore",
            "restore-pg",
        )
        .expect("a namespaced Repository must be refused centrally");
        assert!(msg.contains("Restore `restore-pg`"), "{msg}");
        assert!(msg.contains("kopiur-system"), "{msg}");
        assert!(msg.contains("ClusterRepository"), "{msg}");
        assert!(msg.contains("Fix:"), "{msg}");
        // In its own namespace a run uses whatever repository it resolves.
        for kind in [
            RepositoryKind::Repository,
            RepositoryKind::ClusterRepository,
        ] {
            assert_eq!(
                central_repository_refusal(&RestoreSite::InNamespace, kind, "Restore", "r"),
                None
            );
        }
    }

    /// One namespace cannot opt another into privileged pods: a central mover
    /// runs in the operator namespace, so both must consent.
    #[test]
    fn an_elevated_central_mover_needs_consent_from_both_namespaces() {
        assert_eq!(
            privileged_consent_namespaces("billing", "billing"),
            vec!["billing"]
        );
        assert_eq!(
            privileged_consent_namespaces("billing", "kopiur-system"),
            vec!["billing", "kopiur-system"]
        );
    }

    #[test]
    fn a_repo_only_job_runs_centrally_only_for_a_central_cluster_repository() {
        use kopiur_api::common::RepositoryKind;
        let central = RepoJobSite::Central {
            namespace: "kopiur-system".into(),
        };
        assert_eq!(
            RepoJobSite::of(
                RepositoryKind::ClusterRepository,
                ExecutionMode::Central,
                Some("kopiur-system"),
                false
            ),
            Ok(central.clone())
        );
        assert_eq!(central.namespace("billing"), "kopiur-system");
        assert!(central.is_central());

        // Every other combination stays beside its owner.
        for (kind, mode) in [
            (
                RepositoryKind::ClusterRepository,
                ExecutionMode::InNamespace,
            ),
            (RepositoryKind::Repository, ExecutionMode::InNamespace),
            // A namespaced Repository has no `execution` field; a `Central`
            // here could only be a caller's mistake, and must not move the Job.
            (RepositoryKind::Repository, ExecutionMode::Central),
        ] {
            let site = RepoJobSite::of(kind, mode, Some("kopiur-system"), false).unwrap();
            assert_eq!(site, RepoJobSite::InNamespace, "{kind:?} {mode:?}");
            assert_eq!(site.namespace("billing"), "billing");
        }

        // A Central repository on an install that cannot run movers centrally
        // is an error, never a quiet fall back to the workload namespace.
        let (reason, message) = RepoJobSite::of(
            RepositoryKind::ClusterRepository,
            ExecutionMode::Central,
            Some("kopiur-system"),
            true,
        )
        .unwrap_err();
        assert_eq!(reason, REASON_CENTRAL_UNAVAILABLE);
        assert!(message.contains("namespaced install"), "{message}");
        let (reason, message) = RepoJobSite::of(
            RepositoryKind::ClusterRepository,
            ExecutionMode::Central,
            None,
            false,
        )
        .unwrap_err();
        assert_eq!(reason, REASON_CENTRAL_UNAVAILABLE);
        assert!(message.contains("KOPIUR_NAMESPACE"), "{message}");
        // The shared message carries no trailing period: callers append theirs.
        assert!(!message.ends_with('.'), "{message}");
    }

    #[test]
    fn a_central_policy_job_is_foreign_to_every_owner_but_its_policy() {
        let central = RepoJobSite::Central {
            namespace: "kopiur-system".into(),
        };
        let mut marked = ObjectMeta {
            owner_references: Some(vec![owner("uid-1")]),
            ..Default::default()
        };
        mark_central_policy(&mut marked, "billing", "postgres", "uid-1");
        assert_eq!(marked.owner_references, None);
        assert_eq!(
            marked
                .annotations
                .as_ref()
                .and_then(|a| a.get(CENTRAL_POLICY_ANNOTATION))
                .map(String::as_str),
            Some("billing/postgres")
        );
        let plain = ObjectMeta::default();

        // Its own policy.
        assert!(!central.is_foreign(&marked, is_policys(&marked, "uid-1")));
        // A same-named policy from another namespace, and an in-namespace
        // policy of the operator namespace: neither may count or use it.
        assert!(central.is_foreign(&marked, is_policys(&marked, "uid-2")));
        assert!(RepoJobSite::InNamespace.is_foreign(&marked, is_policys(&marked, "uid-1")));
        // An unmarked Job is an in-namespace owner's, never a central owner's.
        assert!(!RepoJobSite::InNamespace.is_foreign(&plain, false));
        assert!(central.is_foreign(&plain, false));
        // A central backup's or restore's object is not an in-namespace Job either.
        assert!(has_central_mark(&marked));
    }

    #[test]
    fn marking_central_drops_the_owner_reference() {
        let mut meta = ObjectMeta {
            owner_references: Some(vec![owner("uid-1")]),
            ..Default::default()
        };
        mark_central(&mut meta, "billing", "nightly", "uid-1");
        assert_eq!(meta.owner_references, None);
        assert!(is_ours(&meta, "uid-1"));
        assert!(!is_ours(&ObjectMeta::default(), "uid-1"));
    }
}
