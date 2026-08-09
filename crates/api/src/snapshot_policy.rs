//! The `SnapshotPolicy` CRD — the *recipe*. Idempotent; runs nothing on its own.
//! ADR-0001 §3.3, ADR-0003 §4.8.

use crate::backend::NfsVolume;
use crate::common::{
    CredentialProjection, CronSpec, DeletionPolicy, Identity, MoverSpec, PodSelector,
    PvcAccessMode, RepositoryRef, ResolvedIdentity, Retention,
};
use k8s_openapi::api::batch::v1::JobSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, LabelSelector};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What to back up: sources, identity, retention, policy, hooks.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "kopiur.home-operations.com",
    version = "v1alpha1",
    kind = "SnapshotPolicy",
    plural = "snapshotpolicies",
    namespaced,
    status = "SnapshotPolicyStatus",
    shortname = "kopiasp",
    category = "kopiur",
    printcolumn = r#"{"name":"Repository","type":"string","jsonPath":".spec.repository.name"}"#,
    printcolumn = r#"{"name":"Repositories","type":"string","jsonPath":".status.repositorySummary"}"#,
    printcolumn = r#"{"name":"Last-Snapshot","type":"date","jsonPath":".status.lastSuccessfulSnapshot"}"#,
    printcolumn = r#"{"name":"Last-Verified","type":"date","jsonPath":".status.lastVerified"}"#,
    printcolumn = r#"{"name":"Suspended","type":"boolean","jsonPath":".spec.suspend"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
// §15: operator-authored spec-level CEL — exactly one of `repository` /
// `repositories` (apiserver + CI validation, complementing the shared
// validator's [`crate::validate::validate_backup_config`] check). Both are
// optional at the type level so old single-repo objects keep decoding; the
// integer-sum form matches the per-item `Source` rule's cheap-constant style.
#[schemars(extend("x-kubernetes-validations" = [{
    "rule": "(has(self.repository) ? 1 : 0) + (has(self.repositories) ? 1 : 0) == 1",
    "message": "exactly one of repository, repositories"
}]))]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPolicySpec {
    /// Discriminated reference to a `Repository` or `ClusterRepository`.
    /// Mutually exclusive with `repositories` (exactly one of the two is set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryRef>,
    /// Multi-repository fan-out: every listed `Repository`/`ClusterRepository`
    /// receives its own independent backup of each source — one `Snapshot` CR +
    /// one mover Job per (source, repository) pair, so the captures are separate
    /// kopia snapshots, not copies of one another. Identity resolves per-repo
    /// under that repository's `identityDefaults`. Mutually exclusive with
    /// `repository` (exactly one of the two is set) AND with `hooks`: with N
    /// concurrent children the first finisher would run the thaw hook while the
    /// other N-1 movers still read — use a single-repo policy plus a
    /// `SnapshotReplication` when hooks are needed for a second target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(min = 1, max = 8))]
    pub repositories: Vec<RepositoryRef>,
    /// Identity overrides — what kopia records as `username@hostname:path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
    /// What to back up (at least one source; webhook-enforced).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 100))]
    pub sources: Vec<Source>,
    /// How the source volume is captured before kopia reads it: `Snapshot` (default), `Direct`, or `Clone`.
    #[serde(default = "default_copy_method")]
    #[schemars(default = "default_copy_method")]
    pub copy_method: CopyMethod,
    /// `VolumeSnapshotClass` used when `copyMethod` snapshots/clones the source. Absent or
    /// empty both mean auto-select the default class for the source PVC's CSI driver, so a
    /// GitOps-templated value (Flux/Kustomize `${VAR}`) is safe when the variable is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_snapshot_class_name: Option<String>,
    /// Staging knobs for `copyMethod: Snapshot`/`Clone` (e.g. how long to wait for
    /// the CSI capture to become ready before failing the backup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staging: Option<StagingSpec>,
    /// Multi-PVC consistency grouping; `None` opts into independent per-PVC snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_group_by")]
    pub group_by: Option<GroupBy>,
    /// GFS retention, enforced by the operator pruning `Snapshot` CRs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<Retention>,
    /// Default `deletionPolicy` for `Snapshot` CRs created against this config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "recipe_default_deletion_policy")]
    pub default_deletion_policy: Option<DeletionPolicy>,
    /// Compression algorithm + per-extension opt-outs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<Compression>,
    /// Paths/patterns kopia should skip while snapshotting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Files>,
    /// Escape hatch for kopia flags not yet modeled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    /// Backup-side error handling: let a snapshot complete-with-errors instead of failing outright.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_handling: Option<ErrorHandling>,
    /// Upload parallelism (kopia's `--max-parallel-snapshots` / `--max-parallel-file-reads`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<Upload>,
    /// First-class backup verification; opt-in (absent ⇒ no verification runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
    /// Named CEL preconditions evaluated before each backup run; opt-in (absent ⇒
    /// no preflight). A failing check holds the `Snapshot` in `Pending`
    /// (`PreflightFailed`) and, after `timeout`, fails it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight: Option<crate::preflight::PreflightSpec>,
    /// Pause this recipe declaratively (schedules and reconcile skip a suspended policy).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suspend: bool,
    /// Pre/post snapshot hooks that run in the workload, not the mover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Hooks>,
    /// Per-recipe mover overrides (resources, cache, security context).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mover: Option<MoverSpec>,
    /// Opt-in credential-Secret projection into each backup mover's namespace (default off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_projection: Option<CredentialProjection>,
    /// Deletion semantics for the `Snapshot` CRs carrying this recipe's config label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion: Option<PolicyDeletionSpec>,
    /// Per-policy override of automatic adoption for discovered snapshots whose
    /// resolved identity matches this recipe; absent inherits the repository's
    /// `catalog.adoption` (see [`effective_adoption`](crate::common::effective_adoption)).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adoption: Option<crate::common::SnapshotAdoption>,
}

/// The repository target(s) of a `SnapshotPolicy`, as an exactly-one-of view
/// over `spec.repository` / `spec.repositories`. Borrowed so consumers match
/// without cloning; produced by [`policy_repositories`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyRepositories<'a> {
    /// The classic single-repo shape (`spec.repository`).
    Single(&'a RepositoryRef),
    /// The multi-repository fan-out shape (`spec.repositories`, 1–8 entries).
    Multi(&'a [RepositoryRef]),
}

/// THE exactly-one-of resolver for a policy's repository target(s).
///
/// Never panics: a stored CR can carry garbage (both set, or neither — e.g. a
/// write that raced an old CRD schema), so the invalid shapes come back as
/// [`ValidationError::PolicyRepositoryExactlyOne`] for the caller's defensive
/// error path rather than as an `unwrap` in a reconciler.
///
/// ```
/// use kopiur_api::snapshot_policy::{PolicyRepositories, policy_repositories};
///
/// let spec: kopiur_api::SnapshotPolicySpec = serde_json::from_value(serde_json::json!({
///     "repository": { "kind": "Repository", "name": "r" },
///     "sources": [ { "pvc": { "name": "d" } } ],
/// }))
/// .unwrap();
/// assert!(matches!(
///     policy_repositories(&spec),
///     Ok(PolicyRepositories::Single(r)) if r.name == "r"
/// ));
/// ```
pub fn policy_repositories(
    spec: &SnapshotPolicySpec,
) -> Result<PolicyRepositories<'_>, crate::error::ValidationError> {
    match (&spec.repository, spec.repositories.is_empty()) {
        (Some(single), true) => Ok(PolicyRepositories::Single(single)),
        (None, false) => Ok(PolicyRepositories::Multi(&spec.repositories)),
        (Some(_), false) => {
            Err(crate::error::ValidationError::PolicyRepositoryExactlyOne { got: "both" })
        }
        (None, true) => {
            Err(crate::error::ValidationError::PolicyRepositoryExactlyOne { got: "neither" })
        }
    }
}

/// The single repository a policy targets, for consumers whose multi-repo
/// support has not landed yet.
///
/// The multi-repo arm returns [`ValidationError::MultiRepositoryNotYetEnabled`]
/// — the same refusal the admission-time feature gate raises — so a consumer
/// routing this through its defensive validation path fails LOUDLY and
/// consistently instead of silently picking repository #1. Multi-repo
/// consumers land in M8/M10; until the gate lifts (M11) this arm is
/// unreachable at runtime for admitted objects.
pub fn single_repository_ref(
    spec: &SnapshotPolicySpec,
) -> Result<&RepositoryRef, crate::error::ValidationError> {
    match policy_repositories(spec)? {
        PolicyRepositories::Single(r) => Ok(r),
        PolicyRepositories::Multi(_) => {
            Err(crate::error::ValidationError::MultiRepositoryNotYetEnabled)
        }
    }
}

/// Tolerant iterator over every repository ref a policy names — whatever
/// exists, in `repository`-then-`repositories` order, no validity judgment.
/// For any-of predicates (watch mappers, repo-edit guards, tenancy loops)
/// that must not error on a malformed stored CR.
pub fn repository_refs(spec: &SnapshotPolicySpec) -> impl Iterator<Item = &RepositoryRef> {
    spec.repository.iter().chain(spec.repositories.iter())
}

/// Whether this policy uses the multi-repository fan-out shape.
pub fn is_multi_repo(spec: &SnapshotPolicySpec) -> bool {
    !spec.repositories.is_empty()
}

/// The repository a `fromPolicy` restore reads, resolved from the policy's
/// repository set + the restore's optional explicit `spec.repository`.
/// **Pure** — the resolver backstop shared with the M9 webhook mirror, so the
/// two can never disagree.
///
/// - **Explicit selection** — it must be a MEMBER of the policy's repository
///   set (compared by normalized [`repo_key`](crate::common::repo_key); the
///   explicit ref resolves relative to `restore_ns`, the members relative to
///   `policy_ns`), else
///   [`ValidationError`](crate::error::ValidationError::RestoreRepositoryNotInPolicy):
///   a typo'd ref must not silently read a repository the recipe never wrote
///   to. A member ref is returned verbatim (the caller resolves it in the
///   restore's namespace, exactly as an explicit ref always has been).
/// - **No selection, single-repo** — the policy's one repository, verbatim.
/// - **No selection, multi-repo** —
///   [`ValidationError::RestoreRepositorySelectionRequired`](crate::error::ValidationError::RestoreRepositorySelectionRequired),
///   naming every valid choice; repository #1 is never guessed (the N
///   repositories are independent captures that can diverge).
pub fn select_restore_repository(
    policy: &SnapshotPolicySpec,
    policy_name: &str,
    policy_ns: &str,
    explicit: Option<&RepositoryRef>,
    restore_ns: &str,
) -> Result<RepositoryRef, crate::error::ValidationError> {
    use crate::common::repo_key;
    let repos = policy_repositories(policy)?;
    let members: Vec<&RepositoryRef> = match repos {
        PolicyRepositories::Single(r) => vec![r],
        PolicyRepositories::Multi(rs) => rs.iter().collect(),
    };
    let valid = || {
        members
            .iter()
            .map(|m| repo_key(m, policy_ns))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match explicit {
        Some(given) => {
            let given_key = repo_key(given, restore_ns);
            if members.iter().any(|m| repo_key(m, policy_ns) == given_key) {
                Ok(given.clone())
            } else {
                Err(
                    crate::error::ValidationError::RestoreRepositoryNotInPolicy {
                        given: given_key,
                        policy: policy_name.to_string(),
                        valid: valid(),
                    },
                )
            }
        }
        None => match repos {
            PolicyRepositories::Single(r) => Ok(r.clone()),
            PolicyRepositories::Multi(_) => Err(
                crate::error::ValidationError::RestoreRepositorySelectionRequired {
                    policy: policy_name.to_string(),
                    valid: valid(),
                },
            ),
        },
    }
}

/// Deletion semantics for the `Snapshot`s carrying a `SnapshotPolicy`'s config
/// label (sub-object per docs/dev/api-conventions.md §4 so future deletion
/// knobs slot in without API breakage). Mirrors `SnapshotSchedule`'s
/// [`ScheduleDeletionSpec`](crate::snapshot_schedule::ScheduleDeletionSpec).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PolicyDeletionSpec {
    /// Consulted by the Snapshot finalizer when the deletion is external and the
    /// owning `SnapshotPolicy` is gone. Absent resolves to `Retain`.
    #[serde(default = "default_on_policy_delete")]
    #[schemars(default = "default_on_policy_delete")]
    pub on_policy_delete: crate::common::PolicyDeletePolicy,
}

fn default_on_policy_delete() -> crate::common::PolicyDeletePolicy {
    crate::common::PolicyDeletePolicy::Retain
}

/// The effective cascade policy for a `SnapshotPolicy`: `spec.deletion.onPolicyDelete`
/// when the sub-object is present, else `Retain`. (A default nested under an
/// ABSENT optional sub-object does not materialize server-side — every read
/// goes through this resolver.)
pub fn effective_on_policy_delete(
    deletion: Option<&PolicyDeletionSpec>,
) -> crate::common::PolicyDeletePolicy {
    deletion.map(|d| d.on_policy_delete).unwrap_or_default()
}

/// A single backup source; exactly one of `pvc`, `pvcSelector`, `nfs` (webhook-enforced).
// The exactly-one-of rule is written as an integer sum of `has()` ternaries rather
// than `[...].filter(x,x).size()==1`: the apiserver estimates per-item CEL cost ×
// `maxItems`, and a list-construction + lambda `filter` blows the budget on the
// repeating `sources` list. The sum form is a cheap constant per item.
// `Default` is derived purely for construction ergonomics: `Source` is built as an
// exhaustive struct literal in ~20 places, and every added field would otherwise have
// to be spelled out at each one. An all-`None` `Source` is not a valid spec (the CEL
// rule above demands exactly one of pvc/pvcSelector/nfs) and admission rejects it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[schemars(extend("x-kubernetes-validations" = [{
    "rule": "(has(self.pvc) ? 1 : 0) + (has(self.pvcSelector) ? 1 : 0) + (has(self.nfs) ? 1 : 0) == 1",
    "message": "exactly one of pvc, pvcSelector, nfs"
}]))]
#[serde(rename_all = "camelCase")]
pub struct Source {
    /// Single PVC by name. Mutually exclusive with `pvcSelector`/`nfs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pvc: Option<PvcSource>,
    /// Label/namespace selector matching many PVCs. Mutually exclusive with `pvc`/`nfs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pvc_selector: Option<PvcSelector>,
    /// An inline NFS export to back up directly. Mutually exclusive with `pvc`/`pvcSelector`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nfs: Option<NfsVolume>,
    /// Mount the source read-only (default `true`; kopia only ever reads it).
    ///
    /// Set `false` **only** to make `fsGroup` work on the source. The kubelet applies
    /// `fsGroup` by recursively `chgrp`-ing the volume and adding group-write — and it
    /// skips that walk entirely on a read-only mount, which is why a mover
    /// `fsGroup`/`fsGroupChangePolicy` otherwise has no effect here. Under
    /// `copyMethod: Snapshot`/`Clone` the walk rewrites the throwaway staged PVC and
    /// never touches your data. Under `copyMethod: Direct` it rewrites the LIVE volume,
    /// which requires `acknowledgeLiveMutation`.
    ///
    /// Not supported on an `nfs` source: the kubelet does not apply `fsGroup` to
    /// in-tree NFS volumes at all, so a read-write mount would grant nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_source_read_only")]
    pub read_only: Option<bool>,
    /// Acknowledges that `copyMethod: Direct` + `readOnly: false` lets the kubelet
    /// recursively `chgrp` the **live** volume to the mover's `fsGroup` and make it
    /// group-writable — permanently, while the workload is running. Required for that
    /// combination alone.
    ///
    /// Ignored (not rejected) otherwise: it is an acknowledgement, never harmful to
    /// carry, and rejecting a stale one would make switching `copyMethod` between
    /// `Direct` and `Snapshot`/`Clone` a two-step edit in both directions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledge_live_mutation: Option<bool>,
    /// What kopia records as the source path (default `/pvc/<name>`, or the NFS export `path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub source_path_override: Option<String>,
    /// How a `pvcSelector`-matched PVC's source path is derived (`pvcName` vs `pvcNamespacedName`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default = "default_source_path_strategy")]
    pub source_path_strategy: Option<SourcePathStrategy>,
}

/// A single backup source addressed by PVC name.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PvcSource {
    /// Name of the `PersistentVolumeClaim` to back up (in the `SnapshotPolicy`'s namespace).
    pub name: String,
}

/// Selects PVCs across namespaces by label.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PvcSelector {
    /// Restricts the search to specific namespaces; absent means the policy's own namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace_selector: Option<NamespaceSelector>,
    /// Standard Kubernetes label selector matching the PVCs to include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_selector: Option<LabelSelector>,
}

/// Restricts a `PvcSelector` to an explicit set of namespaces.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceSelector {
    /// Exact namespace names to search; empty means the policy's own namespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_names: Vec<String>,
}

/// serde/schemars `default` for [`SnapshotPolicySpec::copy_method`] — **`Snapshot`**.
///
/// `Snapshot` (point-in-time CSI `VolumeSnapshot` staging) is the default because it
/// is **crash-consistent**: kopia reads a frozen point-in-time capture instead of a
/// live, possibly-mid-write PVC, which matters most for databases and other stateful
/// apps. It requires the CSI external-snapshotter stack plus a `VolumeSnapshotClass`
/// for the source's driver. `Direct` (read the live PVC) remains available and is the
/// right choice for non-CSI/static sources (e.g. hostPath, some NFS setups) or when the
/// snapshot stack isn't installed — set `copyMethod: Direct` explicitly to opt in. If
/// the CSI stack is missing under the `Snapshot` default, the operator fails loud: the
/// `Snapshot`/`SnapshotPolicy` status condition and Warning Event spell out exactly
/// what to install or which field to set (see `crates/controller/src/io/staging.rs`).
///
/// A named fn so it backs BOTH `#[serde(default = ...)]` and `#[schemars(default = ...)]`,
/// which is what makes schemars 1 emit a real OpenAPI `default:` in the generated CRD.
fn default_copy_method() -> CopyMethod {
    CopyMethod::Snapshot
}

/// The default OS-artifact exclude set for `Files.ignore_rules` — filesystem/NAS
/// junk that is never intentional user data, so excluding it by default is
/// additive-safe. Per-entry rationale:
///
/// - `/lost+found` — root-anchored ext4/fsck recovery dir. Anchored (leading
///   `/`) so a *nested* user directory named `lost+found` is left alone; only
///   the source root's own fsck dir is excluded.
/// - `System Volume Information`, `$RECYCLE.BIN` — Windows/SMB-client
///   artifacts that show up on samba-share-backed PVCs.
/// - `@eaDir` — Synology NAS extended-attribute/thumbnail metadata junk.
/// - `.snapshot` — NAS-exposed snapshot pseudo-directories (NetApp-style).
///   Deliberately **unanchored** (no leading `/`): these appear at *every*
///   level of a NetApp-backed export, not just the root, and backing one up
///   recursively would multiply the backup size by re-capturing older
///   snapshot generations as regular file data. Flip side: a legitimate
///   directory named `.snapshot` at any depth is also excluded — set
///   `ignoreRules` explicitly if you have one (your list replaces the default).
///
/// A named fn so it backs BOTH `#[serde(default = ...)]` (the common case: an
/// absent `files:` block, handled by the controller glue in
/// `kopiur_mover::workspec` since the apiserver only server-side-defaults
/// NESTED fields when the parent object is present) AND
/// `#[schemars(default = ...)]` (so the default is visible in the generated
/// CRD schema / `kubectl explain`, and applies when `files: {}` is present
/// without `ignoreRules`). ONE source of truth for both layers.
pub fn default_ignore_rules() -> Vec<String> {
    vec![
        "/lost+found".to_string(),
        "System Volume Information".to_string(),
        "$RECYCLE.BIN".to_string(),
        "@eaDir".to_string(),
        ".snapshot".to_string(),
    ]
}

/// Volume snapshot copy method. Closed enum. ADR §3.3.
///
/// ```
/// use kopiur_api::CopyMethod;
///
/// // Defaults to crash-consistent CSI VolumeSnapshot staging.
/// assert_eq!(CopyMethod::default(), CopyMethod::Snapshot);
/// // Serializes as a bare PascalCase string (no external tagging — it has no payload).
/// assert_eq!(serde_json::to_value(CopyMethod::Snapshot).unwrap(), "Snapshot");
/// assert_eq!(serde_json::to_value(CopyMethod::Direct).unwrap(), "Direct");
/// ```
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum CopyMethod {
    /// Point-in-time CSI volume snapshot (the default; requires the CSI snapshot stack + a `VolumeSnapshotClass`).
    #[default]
    Snapshot,
    /// CSI volume clone of the source (opt-in; requires a cloning-capable CSI driver). Mounted per `sources[].readOnly` — read-only by default.
    Clone,
    /// Read the live PVC directly with no intermediate snapshot/clone (opt-in; works on any storage, no CSI required).
    Direct,
}

/// `SnapshotPolicy.spec.staging` — knobs for the CSI capture (`copyMethod:
/// Snapshot`/`Clone`) that runs before the mover. A sub-object so future staging
/// fields slot in without API breakage.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StagingSpec {
    /// How long each staging phase may take before the backup is failed (Go-style
    /// duration like `10m` or `1h`; default `10m`): first the staged
    /// `VolumeSnapshot` becoming `readyToUse` (measured from its creation), then —
    /// on an `Immediate`-binding StorageClass — the staged PVC binding (a fresh
    /// budget measured from the PVC's creation, covering the CSI restore/clone).
    /// A transient CSI/snapshot-controller error during either wait is retried,
    /// never fatal on its own — only this deadline fails staging. A zero duration
    /// (`0`/`0s`) waits indefinitely. Raise this for backends whose snapshots or
    /// clones take long (e.g. cloud snapshots of large volumes, CephFS full clones
    /// of small-file-heavy volumes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// StorageClass for the **staged PVC** — the temporary PVC restored from the
    /// CSI `VolumeSnapshot` (`copyMethod: Snapshot`) or cloned from the source
    /// (`copyMethod: Clone`). Absent ⇒ the staged PVC copies the source PVC's
    /// class. Must belong to the **same CSI driver** as the source (staging fails
    /// fast on a mismatch). Flagship use: a rook-ceph CephFS class with
    /// `backingSnapshot: "true"`, which mounts the snapshot shallowly
    /// (metadata-only, near-instant, read-only) instead of running a full
    /// subvolume clone that can take many minutes on small-file-heavy volumes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    /// Access modes for the staged PVC. Empty ⇒ copy the source PVC's modes.
    /// `[ReadOnlyMany]` pairs with snapshot-backed read-only classes (e.g. CephFS
    /// `backingSnapshot`); the mover mounts the staged PVC read-only to match, and
    /// rejects it at admission if a source sets `readOnly: false` (a read-only stage
    /// cannot be mounted read-write).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access_modes: Vec<PvcAccessMode>,
}

/// Multi-PVC grouping strategy. Defaults to a consistent group snapshot across
/// all PVCs; set `None` *explicitly* to accept independent per-PVC snapshots,
/// because a silent per-PVC fallback would produce inconsistent backups.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum GroupBy {
    /// Consistent group snapshot across all PVCs (default for multi-PVC).
    #[default]
    VolumeGroupSnapshot,
    /// Opt into independent per-PVC snapshots.
    None,
}

/// schemars default for `PvcSnapshotPolicy::group_by` — the consistent group
/// snapshot. Returns the field's `Option` type so schemars emits the schema
/// `default:` (`VolumeGroupSnapshot`) for `kubectl explain`.
fn default_group_by() -> Option<GroupBy> {
    Some(GroupBy::VolumeGroupSnapshot)
}

/// How a selector-matched PVC's source path is derived. Only relevant for
/// `pvcSelector` sources, where one recipe expands to many PVCs and each needs a
/// distinct kopia source path. Defaults to `PvcName`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum SourcePathStrategy {
    /// Path derived from the PVC name alone (default).
    #[default]
    PvcName,
    /// Path derived from `<namespace>/<name>` to disambiguate same-named PVCs across namespaces.
    PvcNamespacedName,
}

/// schemars default for `PvcSnapshotPolicy::source_path_strategy` — `PvcName`.
/// Returns the field's `Option` type so schemars emits the schema `default:`.
fn default_source_path_strategy() -> Option<SourcePathStrategy> {
    Some(SourcePathStrategy::PvcName)
}

/// schemars default for `Source::read_only` — a backup source is read-only unless the
/// user asks otherwise. Returns the field's `Option` type so schemars emits the schema
/// `default: true` for `kubectl explain`. Paired with `source_read_only()`, which
/// resolves absent to exactly this value at the mount site — the pairing is what makes
/// a schema default safe to advertise (see `Repository`'s health defaults).
fn default_source_read_only() -> Option<bool> {
    Some(true)
}

/// Whether a source is mounted read-only. THE resolver for `Source::read_only`'s
/// absent case, so the CRD's advertised `default: true` and the mount agree by
/// construction rather than by coincidence.
pub fn source_read_only(source: &Source) -> bool {
    source.read_only.unwrap_or(true)
}

/// Whether this source's mount lets the kubelet rewrite the **live** workload volume:
/// a writable mount with no staging in front of it.
///
/// `copyMethod: Snapshot`/`Clone` interpose a throwaway staged PVC, so the kubelet's
/// recursive `fsGroup` chgrp lands on a copy that is deleted when the run ends. Only
/// `Direct` mounts the workload's own PVC, where that same walk permanently rewrites
/// group ownership on production data. Pure, and the single definition of the hazard.
///
/// An `nfs` source is excluded, and not merely because [`validate_source`] rejects a
/// writable one anyway: the kubelet does not apply `fsGroup` to in-tree NFS volumes at
/// all, so no walk ever happens and this predicate's premise is simply false there.
/// Answering `true` would also make admission emit two errors for one mistake, the
/// second of them advice — "set `acknowledgeLiveMutation`" — that could never make the
/// configuration valid.
///
/// [`validate_source`]: crate::validate::validate_source
pub fn source_mutates_live_volume(copy_method: CopyMethod, source: &Source) -> bool {
    matches!(copy_method, CopyMethod::Direct)
        && !source_read_only(source)
        && (source.pvc.is_some() || source.pvc_selector.is_some())
}

/// schemars default for `PvcSnapshotPolicy::default_deletion_policy` — `Delete`,
/// the deletion policy produced `Snapshot` CRs inherit. Returns the field's
/// `Option` type so schemars emits the schema `default:`.
fn recipe_default_deletion_policy() -> Option<crate::common::DeletionPolicy> {
    Some(crate::common::DeletionPolicy::Delete)
}

/// Compression policy.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Compression {
    /// kopia compressor name (e.g. `zstd`); absent leaves kopia's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressor: Option<String>,
    /// Filename globs to leave uncompressed (e.g. already-compressed media).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub never_compress: Vec<String>,
}

/// File-ignore policy.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Files {
    /// Filename/path globs to exclude from the snapshot (e.g. `*.tmp`, `*/cache/*`).
    /// Absent ⇒ [`default_ignore_rules`] (OS-artifact junk: `/lost+found`,
    /// `System Volume Information`, `$RECYCLE.BIN`, `@eaDir`, `.snapshot`). An
    /// explicit list REPLACES the default wholesale (re-add any entries you
    /// still want); explicit `ignoreRules: []` opts fully out. NOT
    /// `skip_serializing_if` — an explicit empty list must round-trip as `[]`,
    /// not vanish back to "absent" (which would silently resurrect the
    /// default on the next parse).
    #[serde(default = "default_ignore_rules")]
    #[schemars(default = "default_ignore_rules")]
    pub ignore_rules: Vec<String>,
    /// Honor `CACHEDIR.TAG`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_cache_dirs: bool,
    /// Skip taking a new snapshot when the source is identical to the previous one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_identical_snapshots: bool,
}

/// Backup-side error-handling policy: let kopia complete a snapshot with errors rather than aborting.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorHandling {
    /// Continue the snapshot when a file cannot be read (`--ignore-file-errors`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_file_errors: bool,
    /// Continue the snapshot when a directory cannot be read (`--ignore-dir-errors`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_dir_errors: bool,
    /// Continue past entries of unknown type (`--ignore-unknown-types`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignore_unknown_types: bool,
    /// Abort the snapshot at the first error instead of collecting and
    /// continuing (`snapshot create --fail-fast`; kopia default: false). This
    /// is a `snapshot create` argv flag, not a `policy set` knob, but lives
    /// beside its semantic opposites (`ignore*Errors`) for discoverability.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fail_fast: bool,
}

/// Upload parallelism (kopia's upload policy); absent knobs leave kopia's default.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Upload {
    /// `--max-parallel-snapshots`: how many sources snapshot concurrently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel_snapshots: Option<i64>,
    /// `--max-parallel-file-reads`: file-read concurrency within a snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel_file_reads: Option<i64>,
    /// `snapshot create --upload-limit-mb`: abort the snapshot once this many
    /// MB have been uploaded (kopia default: 0 — unlimited). Named `limitMb`
    /// rather than `uploadLimitMb` to avoid the `upload.uploadLimitMb` stutter;
    /// like `failFast`, this is a `snapshot create` argv flag, not a `policy
    /// set` knob, but lives here beside its parallelism siblings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_mb: Option<i64>,
}

/// First-class backup verification proving snapshots are restorable; opt-in, with quick and deep tiers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Verification {
    /// Quick (blob-level) verification tier; absent ⇒ no quick verification. Its cron
    /// lives under `quick.schedule` (matching `deep.schedule`), see [`QuickVerification`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick: Option<QuickVerification>,
    /// Schedule + knobs for the rarer scratch-restore test; absent ⇒ no deep verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep: Option<DeepVerification>,
    /// CEL pass/fail predicate over the verify result; applies to both tiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_expr: Option<String>,
    /// How many files `quick` verifies fully (`--verify-files-percent`); absent leaves kopia's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_files_percent: Option<u8>,
}

/// Quick (blob-level) verification tier: schedule for the frequent `kopia snapshot verify`.
///
/// A wrapper so this tier's shape matches `deep` — the cron lives at
/// `quick.schedule.cron` (GitHub #174). `schedule` is deliberately `Option` for
/// decode-tolerance: an already-persisted old-shape `quick: { cron: ... }` object
/// still decodes (serde ignores the unknown `cron` key) as `schedule: None` rather
/// than failing typed serde — a hard decode failure would wedge the SnapshotPolicy
/// reflector and poison SnapshotPolicy admission cluster-wide. New writes with the
/// old shape are rejected at admission by the shared validator, which points at the
/// move. A persisted `schedule: None` means the quick tier is disabled until updated.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuickVerification {
    /// Cron + jitter + timezone for the frequent blob-level verify; absent ⇒ quick tier disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<CronSpec>,
    /// `--parallel`: verification parallelism (kopia default: 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// `--file-parallelism`: parallelism for file verification (kopia default: unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_parallelism: Option<u32>,
    /// `--file-queue-length`: queue length for file verification (kopia default: 20000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_queue_length: Option<u32>,
    /// `--max-errors`: stop after this many errors (kopia default: 0, meaning stop
    /// at the first error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_errors: Option<u32>,
}

/// Deep (scratch-restore) verification: restore the latest snapshot into an ephemeral volume, then discard.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeepVerification {
    /// Cron + jitter for the deep restore-test (e.g. weekly).
    pub schedule: CronSpec,
    /// StorageClass for the ephemeral scratch PVC; absent uses the cluster default (only with `capacity`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    /// Size of the ephemeral scratch PVC (e.g. `10Gi`); absent falls back to a node-ephemeral `emptyDir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<String>,
    /// `restore --parallel`: restore parallelism for the scratch-restore (deep verify
    /// IS a restore under the hood); absent leaves kopia's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
}

/// Pre/post snapshot hook lists.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Hooks {
    /// Hooks run (in order) before the snapshot is taken — e.g. quiescing a database.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before_snapshot: Vec<Hook>,
    /// Hooks run (in order) after the snapshot completes — e.g. resuming the workload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after_snapshot: Vec<Hook>,
}

/// One of three hook forms. Externally-tagged: the wire shape is
/// `{ workloadExec: {...} }`, `{ runJob: {...} }`, or `{ httpRequest: {...} }`,
/// and exactly one form is present.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Hook {
    /// `kubectl exec`-style into a matched workload pod/container (the default form).
    WorkloadExec(WorkloadExecHook),
    /// Full `JobSpec` run as a one-shot Job (k8up `PreBackupPod` analog).
    RunJob(Box<RunJobHook>),
    /// Typed POST to a URL for cross-system orchestration.
    HttpRequest(HttpRequestHook),
}

impl Hook {
    /// Stable discriminant string for status/metrics — one of `"WorkloadExec"`,
    /// `"RunJob"`, or `"HttpRequest"`.
    ///
    /// ```
    /// use kopiur_api::snapshot_policy::{Hook, HttpRequestHook};
    ///
    /// let hook = Hook::HttpRequest(HttpRequestHook {
    ///     url: "https://example/notify".into(),
    ///     method: None,
    ///     body: None,
    ///     headers: Vec::new(),
    ///     timeout: None,
    ///     continue_on_failure: false,
    /// });
    /// assert_eq!(hook.kind_str(), "HttpRequest");
    /// ```
    pub fn kind_str(&self) -> &'static str {
        match self {
            Hook::WorkloadExec(_) => "WorkloadExec",
            Hook::RunJob(_) => "RunJob",
            Hook::HttpRequest(_) => "HttpRequest",
        }
    }
}

/// `kubectl exec`-style hook into a matched workload pod/container.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadExecHook {
    /// Selects the workload pod/container to exec into (flattened onto the hook).
    #[serde(flatten)]
    pub selector: PodSelector,
    /// Command + args to run inside the selected container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Max time to wait for the command (Go duration string, e.g. `2m`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// If `true`, a failed hook does not abort the backup (default: abort).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub continue_on_failure: bool,
}

/// A hook that materializes a full one-shot Job (k8up `PreBackupPod` analog).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunJobHook {
    /// The full Kubernetes `JobSpec` to run.
    #[schemars(schema_with = "crate::schema::preserve_unknown_object")]
    pub job_spec: JobSpec,
    /// Max time to wait for the Job to complete (Go duration string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// If `true`, a failed Job does not abort the backup (default: abort).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub continue_on_failure: bool,
}

/// One HTTP header sent with an `httpRequest` hook.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HttpHeader {
    /// Header name (case-insensitive; RFC 7230 token, e.g. `Content-Type`).
    pub name: String,
    /// Header value.
    pub value: String,
}

/// A hook that issues an HTTP request for cross-system orchestration.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequestHook {
    /// Target URL to call.
    pub url: String,
    /// HTTP method (default `POST`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Optional request body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Additional request headers (e.g. `Content-Type: application/json`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HttpHeader>,
    /// Max time to wait for the response (Go duration string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// If `true`, a failed request does not abort the backup (default: abort).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub continue_on_failure: bool,
}

/// Observed state of a `SnapshotPolicy`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPolicyStatus {
    /// `metadata.generation` last reconciled, for staleness detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// What would be passed to kopia — pinned at admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedPolicy>,
    /// Summary of GFS retention pruning against this config's `Snapshot` CRs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RetentionSummary>,
    /// Summary of automatic adoption of discovered snapshots into this recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adoption: Option<AdoptionSummary>,
    /// RFC3339 timestamp of the most recent successful child `Snapshot` from this recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_snapshot: Option<String>,
    /// RFC3339 timestamp of the most recent successful verification (any tier).
    /// Single-repo: stamped directly by the verify mover. Multi-repo: computed
    /// by the controller as the MINIMUM `lastVerified` across the CURRENT
    /// repositories ("everything is verified as of T"), absent until every
    /// current repository has verified at least once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified: Option<String>,
    /// Per-repository verification records for a multi-repository policy
    /// (#368): one entry per CURRENT `spec.repositories` member, maintained by
    /// the controller (single writer — entries for repositories no longer in
    /// the spec are pruned). Empty (elided) for the single-repo shape, whose
    /// wire stays byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification: Vec<RepoVerification>,
    /// Internal write channel for per-repository verification (#368): RFC3339
    /// markers keyed by the normalized repository key
    /// ([`repo_key`](crate::common::repo_key)). Each verify mover merge-patches
    /// ONLY its own key — a JSON merge patch merges map keys, so two concurrent
    /// per-repo verifies can never clobber one another (a Vec would be replaced
    /// wholesale). The controller folds these into `verification` on its next
    /// pass and prunes keys for repositories no longer in the spec. Never
    /// written for the single-repo shape.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub verification_stamps: std::collections::BTreeMap<String, String>,
    /// Human-readable summary of the policy's repository target(s) for the
    /// `Repositories` print column: the comma-joined repository names (the one
    /// name for the single-repo shape), capped near a kubectl column width
    /// with a `+N` overflow marker. Written by the controller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_summary: Option<String>,
    /// Standard Kubernetes conditions (e.g. `RepositoryReachable`, `GroupSnapshotSupported`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// One repository's verification record on a multi-repository policy
/// (`status.verification`). Entry-keyed by the (normalized) repository ref so
/// per-repo verify results never collapse into one flat timestamp.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoVerification {
    /// The repository this record covers, normalized
    /// ([`normalized_repository_ref`](crate::common::normalized_repository_ref))
    /// so it re-resolves from anywhere.
    pub repository: RepositoryRef,
    /// RFC3339 timestamp of the most recent successful verification (any tier)
    /// against THIS repository; absent until its first successful verify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified: Option<String>,
}

/// The recipe as kopia would see it, pinned at admission and never re-rendered.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedPolicy {
    /// The resolved `username@hostname` identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ResolvedIdentity>,
    /// The concrete PVCs + source paths after selector expansion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<ResolvedPolicySource>,
    /// Per-repository resolution for a MULTI-repository policy
    /// (`spec.repositories`): one entry per member, each carrying the identity
    /// resolved under THAT repository's `identityDefaults` (the unit of
    /// identity is the `(repository, identity)` pair — N members means N
    /// independent kopia lineages). Empty — and elided from the wire — for the
    /// classic single-repo shape, whose resolution stays in the top-level
    /// `identity`/`sources` fields exactly as before this field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<ResolvedPolicyRepository>,
}

/// One member repository's resolution within a multi-repository policy: which
/// repository, and what kopia identity this policy resolves to **under that
/// repository's `identityDefaults`**. Consumed by the admission fork guard as
/// the per-repo baseline (`repo_key` → previously-resolved identity), so an
/// edit that would re-identify ONE member's lineage is caught even though the
/// other members are unaffected.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedPolicyRepository {
    /// The member repository this entry resolves for (by value, as listed in
    /// `spec.repositories`).
    pub repository: RepositoryRef,
    /// The `username@hostname` identity resolved under this repository's
    /// `identityDefaults`; absent when it could not be resolved (the guard
    /// treats an absent baseline as "no baseline" and degrades to allow for
    /// that member only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ResolvedIdentity>,
}

/// One resolved source — a concrete PVC and the path kopia records for it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedPolicySource {
    /// `namespace/name` of the PVC, as kopia sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pvc: Option<String>,
    /// The source path kopia records for this PVC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// Summary of the most recent GFS retention prune for a `SnapshotPolicy`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RetentionSummary {
    /// CRs currently inside the GFS window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_snapshot_count: Option<i64>,
    /// RFC3339 timestamp of the last prune pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prune_at: Option<String>,
    /// Number of `Snapshot` CRs deleted by the last prune pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prune_deleted: Option<i64>,
}

/// Summary of the most recent automatic adoption pass for a `SnapshotPolicy` —
/// discovered snapshots whose resolved identity matched this recipe and were
/// re-attached (see `Origin::Adopted`), plus an on-demand re-scan request/ack
/// pair mirroring the repository-level `catalog-scan-requested-at` annotation
/// contract.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdoptionSummary {
    /// RFC3339 timestamp of the last adoption pass that adopted at least one snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_adoption_at: Option<String>,
    /// Number of discovered `Snapshot` CRs adopted by the last adoption pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_adopted_count: Option<u32>,
    /// Running total of `Snapshot` CRs ever adopted into this recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_adopted: Option<u64>,
    /// Identity-matching discovered snapshots the last adoption pass left
    /// discovered because `spec.retention` would prune them immediately under
    /// the effective `deletionPolicy` (`Retain`/`Orphan` — a CR-only prune that
    /// would re-discover and re-adopt forever). `0`/absent when nothing was
    /// withheld. See the `AdoptionSkippedByRetention` event for the levers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_by_retention: Option<u32>,
    /// RFC3339 token echoing an in-flight on-demand adoption scan request for
    /// this policy's identity; cleared once honored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_requested_at: Option<String>,
    /// The resolved kopia identity the requested scan was scoped to, pinned at
    /// request time so a later identity-changing edit can't retarget an
    /// in-flight scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_requested_identity: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::RepositoryKind;
    use crate::testutil::from_yaml;
    use kube::core::CustomResourceExt;

    #[test]
    fn resolved_policy_single_repo_status_wire_is_byte_identical() {
        // Golden: a single-repo policy's ResolvedPolicy serializes EXACTLY as
        // it did before `repositories` existed — the empty vec is elided — so
        // stored single-repo statuses round-trip byte-identically.
        let resolved = ResolvedPolicy {
            identity: Some(crate::common::ResolvedIdentity {
                username: "pg".into(),
                hostname: "billing".into(),
                source_path: Some("/pvc/data".into()),
            }),
            sources: vec![ResolvedPolicySource {
                pvc: Some("billing/data".into()),
                source_path: Some("/pvc/data".into()),
            }],
            repositories: vec![],
        };
        let wire = serde_json::to_value(&resolved).expect("serializes");
        assert_eq!(
            wire,
            serde_json::json!({
                "identity": {
                    "username": "pg",
                    "hostname": "billing",
                    "sourcePath": "/pvc/data",
                },
                "sources": [ { "pvc": "billing/data", "sourcePath": "/pvc/data" } ],
            })
        );

        // …and a pre-feature stored status (no `repositories` key) decodes to
        // the empty vec, not an error.
        let decoded: ResolvedPolicy = serde_json::from_value(wire).expect("decodes");
        assert!(decoded.repositories.is_empty());
    }

    #[test]
    fn resolved_policy_per_repo_entries_round_trip() {
        let resolved = ResolvedPolicy {
            identity: None,
            sources: vec![],
            repositories: vec![
                ResolvedPolicyRepository {
                    repository: RepositoryRef {
                        kind: RepositoryKind::Repository,
                        name: "nas".into(),
                        namespace: None,
                    },
                    identity: Some(crate::common::ResolvedIdentity {
                        username: "pg".into(),
                        hostname: "billing".into(),
                        source_path: None,
                    }),
                },
                ResolvedPolicyRepository {
                    repository: RepositoryRef {
                        kind: RepositoryKind::ClusterRepository,
                        name: "offsite".into(),
                        namespace: None,
                    },
                    // Unresolvable member: entry present, identity elided.
                    identity: None,
                },
            ],
        };
        let wire = serde_json::to_value(&resolved).expect("serializes");
        assert_eq!(
            wire,
            serde_json::json!({
                "repositories": [
                    {
                        "repository": { "kind": "Repository", "name": "nas" },
                        "identity": { "username": "pg", "hostname": "billing" },
                    },
                    { "repository": { "kind": "ClusterRepository", "name": "offsite" } },
                ],
            })
        );
        let decoded: ResolvedPolicy = serde_json::from_value(wire).expect("decodes");
        assert_eq!(decoded, resolved);
    }

    #[test]
    fn snapshot_policy_crd_metadata_is_correct() {
        let crd = SnapshotPolicy::crd();
        assert_eq!(crd.spec.group, "kopiur.home-operations.com");
        assert_eq!(crd.spec.names.kind, "SnapshotPolicy");
        assert_eq!(crd.spec.names.plural, "snapshotpolicies");
        assert_eq!(
            crd.spec.names.short_names.as_deref(),
            Some(&["kopiasp".to_string()][..])
        );
        assert_eq!(crd.spec.scope, "Namespaced");
        assert_eq!(crd.spec.versions[0].name, "v1alpha1");
    }

    #[test]
    fn copy_method_carries_static_openapi_default_in_crd() {
        // copyMethod must carry a real schema `default: Snapshot` so it appears in
        // `kubectl explain` / the stored object and GitOps stops thrashing. `Snapshot`
        // (crash-consistent CSI staging) is the community-preferred default; `Direct` /
        // `Clone` are opt-in.
        let crd = SnapshotPolicy::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let default = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["copyMethod"]["default"];
        assert_eq!(
            default, "Snapshot",
            "copyMethod must emit `default: Snapshot` in the CRD schema; got {default:?}"
        );
    }

    #[test]
    fn staging_timeout_round_trips_and_defaults_to_absent() {
        // Absent staging parses to None (runtime default 10m applies in the
        // controller) and is skip-elided on the wire.
        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\n",
        );
        assert_eq!(spec.staging, None);
        let json = serde_json::to_value(&spec).unwrap();
        assert!(
            json.get("staging").is_none(),
            "absent staging must be elided"
        );

        // A set timeout round-trips through the cluster's parse path.
        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\n\
             sources: [ { pvc: { name: d } } ]\n\
             staging: { timeout: 30m }\n",
        );
        assert_eq!(
            spec.staging,
            Some(StagingSpec {
                timeout: Some("30m".to_string()),
                ..Default::default()
            })
        );
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["staging"]["timeout"], "30m");
    }

    #[test]
    fn staging_overrides_round_trip_and_are_elided_when_absent() {
        // The staged-PVC override pair (storageClassName + accessModes) round-trips
        // through the cluster's parse path; absent fields are skip-elided so
        // "inherit from the source PVC" stays representable as absence.
        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\n\
             sources: [ { pvc: { name: d } } ]\n\
             staging: { timeout: 30m, storageClassName: cephfs-shallow, accessModes: [ReadOnlyMany] }\n",
        );
        let st = spec.staging.as_ref().unwrap();
        assert_eq!(st.storage_class_name.as_deref(), Some("cephfs-shallow"));
        assert_eq!(st.access_modes, vec![PvcAccessMode::ReadOnlyMany]);
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["staging"]["storageClassName"], "cephfs-shallow");
        assert_eq!(
            json["staging"]["accessModes"],
            serde_json::json!(["ReadOnlyMany"])
        );

        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\n\
             sources: [ { pvc: { name: d } } ]\n\
             staging: { timeout: 30m }\n",
        );
        let json = serde_json::to_value(&spec).unwrap();
        assert!(json["staging"].get("storageClassName").is_none());
        assert!(json["staging"].get("accessModes").is_none());
    }

    #[test]
    fn staging_access_modes_legacy_value_decodes_to_unknown_not_an_error() {
        // Graceful-decode contract: a non-canonical stored mode deserializes into
        // `Unknown` (rejected later by the shared validator, per-CR) instead of a
        // serde error that would wedge the typed watcher for every SnapshotPolicy.
        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\n\
             sources: [ { pvc: { name: d } } ]\n\
             staging: { accessModes: [ReadWriteOnze] }\n",
        );
        assert_eq!(
            spec.staging.unwrap().access_modes,
            vec![PvcAccessMode::Unknown("ReadWriteOnze".into())]
        );
    }

    #[test]
    fn staging_access_modes_render_a_closed_enum_in_the_crd_schema() {
        // First `Vec<unit-enum>` in the API crate: pin that the generated CRD
        // schema is `items: {type: string, enum: [...]}` with exactly the four
        // canonical modes — and does NOT leak the legacy-decode `Unknown` variant.
        let crd = SnapshotPolicy::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let items = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["staging"]["properties"]["accessModes"]["items"];
        assert_eq!(
            items["type"], "string",
            "items must be strings; got {items}"
        );
        assert_eq!(
            items["enum"],
            serde_json::json!([
                "ReadWriteOnce",
                "ReadOnlyMany",
                "ReadWriteMany",
                "ReadWriteOncePod"
            ]),
            "items enum must be exactly the canonical modes; got {items}"
        );
    }

    #[test]
    fn copy_method_defaults_to_snapshot_when_absent() {
        // A bare value with a serde default: an omitted copyMethod parses to Snapshot (the
        // crash-consistent CSI-staged behavior).
        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\n",
        );
        assert_eq!(spec.copy_method, CopyMethod::Snapshot);
        // And it serializes (not skip-elided), so the materialized value round-trips.
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["copyMethod"], "Snapshot");
    }

    /// The 5-entry OS-artifact default set, in the fixed order `default_ignore_rules`
    /// returns it — shared by every assertion below so the list itself has one
    /// source of truth in the test file too.
    fn expected_default_ignore_rules() -> Vec<String> {
        vec![
            "/lost+found".to_string(),
            "System Volume Information".to_string(),
            "$RECYCLE.BIN".to_string(),
            "@eaDir".to_string(),
            ".snapshot".to_string(),
        ]
    }

    #[test]
    fn files_ignore_rules_carries_static_openapi_default_in_crd() {
        // `files.ignoreRules` must carry a real schema `default:` (the 5-entry
        // OS-artifact set) so it appears in `kubectl explain`. Mirrors
        // `copy_method_carries_static_openapi_default_in_crd`.
        let crd = SnapshotPolicy::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let default = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["files"]["properties"]["ignoreRules"]["default"];
        let want: Vec<serde_json::Value> = expected_default_ignore_rules()
            .into_iter()
            .map(serde_json::Value::String)
            .collect();
        assert_eq!(
            default,
            &serde_json::Value::Array(want),
            "files.ignoreRules must emit the 5-entry OS-artifact `default:` in the CRD schema; got {default:?}"
        );
    }

    #[test]
    fn ignore_rules_defaults_when_files_block_absent_entirely() {
        // The load-bearing case: apiserver server-side-defaulting only fires for
        // NESTED fields when the parent object is present, so a spec that omits
        // `files:` altogether never gets `Files.ignoreRules`'s schema default
        // applied by the apiserver. The *serde* default on `Files::ignore_rules`
        // only helps once `files: {}` exists — it can't fire on a wholly-`None`
        // `spec.files`. This asserts the glue tier's contract: the mover work-spec
        // seam (`kopiur_mover::workspec::PolicyArgsSpec::from_policy`) is the layer
        // that must apply `default_ignore_rules()` for THIS shape; see the mover
        // crate's `workspec` tests for that half.
        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\n",
        );
        assert!(
            spec.files.is_none(),
            "a spec omitting `files:` entirely must parse to `None`, not a defaulted `Files`"
        );
    }

    #[test]
    fn ignore_rules_defaults_when_files_block_present_but_empty() {
        // `files: {}` (parent present, `ignoreRules` absent): the serde default
        // DOES fire here, and this is also what the schemars `default:` covers for
        // apiserver server-side-defaulting.
        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\nfiles: {}\n",
        );
        let files = spec.files.expect("files: {} must parse to Some(Files)");
        assert_eq!(files.ignore_rules, expected_default_ignore_rules());
    }

    #[test]
    fn ignore_rules_explicit_empty_list_opts_out_and_round_trips() {
        // Regression test for the opt-out subtlety: an explicit `ignoreRules: []`
        // must deserialize as present-empty (serde defaults only fire when the KEY
        // is ABSENT, not when it's present-and-empty) and — critically — must
        // round-trip back through serialize/deserialize as `[]`, not vanish to
        // "absent" and silently resurrect the default. This is why `ignore_rules`
        // does NOT carry `skip_serializing_if`.
        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\nfiles: { ignoreRules: [] }\n",
        );
        let files = spec
            .files
            .as_ref()
            .expect("files: {...} must parse to Some(Files)");
        assert!(
            files.ignore_rules.is_empty(),
            "explicit `ignoreRules: []` must opt fully out, got {:?}",
            files.ignore_rules
        );

        // The round-trip: serialize back to JSON, the `ignoreRules` key must still
        // be PRESENT (as `[]`), not omitted.
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(
            json["files"]["ignoreRules"],
            serde_json::json!([]),
            "an explicit empty ignoreRules must serialize as `[]`, not be omitted \
             (omission would deserialize back to the 5-entry default)"
        );

        // And re-parsing that JSON must still yield the empty, opted-out list —
        // not the default reappearing.
        let reparsed: SnapshotPolicySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
        assert!(reparsed.files.expect("files").ignore_rules.is_empty());
    }

    #[test]
    fn ignore_rules_explicit_custom_list_replaces_default_wholesale() {
        // An explicit non-empty list REPLACES the default outright — it is not
        // merged/appended. Re-adding a default entry you still want is on the
        // user (documented in docs/backups.md).
        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\nfiles: { ignoreRules: [\"*.tmp\", \"lost+found\"] }\n",
        );
        let files = spec.files.expect("files");
        assert_eq!(
            files.ignore_rules,
            vec!["*.tmp".to_string(), "lost+found".to_string()]
        );
    }

    #[test]
    fn backup_config_roundtrip_matches_adr_shape() {
        // Mirrors ADR-0001 §3.3.
        let yaml = r#"
repository:
  kind: Repository
  name: nas-primary
  namespace: backups
identity:
  username: "postgres-data"
  hostname: "billing"
sources:
  - pvc: { name: postgres-data }
    sourcePathOverride: /data
copyMethod: Snapshot
volumeSnapshotClassName: csi-snap-class
groupBy: VolumeGroupSnapshot
retention:
  keepLatest: 10
  keepDaily: 14
defaultDeletionPolicy: Delete
compression:
  compressor: zstd
  neverCompress: ["*.zip", "*.gz", "*.mp4"]
files:
  ignoreRules: ["*.tmp", "*/cache/*", "lost+found"]
  ignoreCacheDirs: true
  ignoreIdenticalSnapshots: true
extraArgs: []
hooks:
  beforeSnapshot:
    - workloadExec:
        podSelector: { matchLabels: { app: postgres } }
        container: postgres
        command: ["pg_start_backup", "snap"]
        timeout: 2m
  afterSnapshot:
    - workloadExec:
        podSelector: { matchLabels: { app: postgres } }
        container: postgres
        command: ["pg_stop_backup"]
        timeout: 2m
mover:
  resources:
    requests: { cpu: 250m, memory: 512Mi }
    limits: { cpu: "2", memory: 4Gi }
  cache:
    capacity: 16Gi
    storageClassName: fast-ssd
  securityContext:
    runAsUser: 1000
    runAsGroup: 1000
    runAsNonRoot: true
    allowPrivilegeEscalation: false
    capabilities: { drop: ["ALL"] }
    seccompProfile: { type: RuntimeDefault }
  podSecurityContext:
    fsGroup: 1000
    fsGroupChangePolicy: OnRootMismatch
"#;
        let spec: SnapshotPolicySpec = from_yaml(yaml);
        let repo = spec.repository.as_ref().expect("repository");
        assert_eq!(repo.kind, RepositoryKind::Repository);
        assert_eq!(repo.name, "nas-primary");
        assert_eq!(spec.sources.len(), 1);
        assert_eq!(spec.sources[0].pvc.as_ref().unwrap().name, "postgres-data");
        assert_eq!(
            spec.sources[0].source_path_override.as_deref(),
            Some("/data")
        );
        assert_eq!(spec.copy_method, CopyMethod::Snapshot);
        assert_eq!(spec.group_by, Some(GroupBy::VolumeGroupSnapshot));
        assert_eq!(spec.default_deletion_policy, Some(DeletionPolicy::Delete));
        let comp = spec.compression.as_ref().unwrap();
        assert_eq!(comp.compressor.as_deref(), Some("zstd"));
        let files = spec.files.as_ref().unwrap();
        assert_eq!(files.ignore_rules.len(), 3);
        assert!(files.ignore_cache_dirs);
        assert!(spec.extra_args.is_empty());
        let hooks = spec.hooks.as_ref().unwrap();
        assert_eq!(hooks.before_snapshot.len(), 1);
        assert_eq!(hooks.before_snapshot[0].kind_str(), "WorkloadExec");
        // Both the container- and pod-level security contexts round-trip on the mover.
        let mover = spec.mover.as_ref().expect("mover");
        assert_eq!(
            mover.security_context.as_ref().and_then(|s| s.run_as_user),
            Some(1000)
        );
        assert_eq!(
            mover.pod_security_context.as_ref().and_then(|p| p.fs_group),
            Some(1000)
        );
        // Container UID/GID match + fsGroup is unprivileged (no namespace opt-in).
        assert!(!mover.requires_privilege());

        let json = serde_json::to_value(&spec).expect("serialize");
        let reparsed: SnapshotPolicySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn credential_projection_roundtrip() {
        // Opt-in projection now lives on the recipe (SnapshotPolicy), parses the
        // cluster's way, and round-trips.
        let yaml = r#"
repository: { kind: ClusterRepository, name: shared }
sources:
  - pvc: { name: data }
retention: { keepLatest: 5 }
credentialProjection:
  enabled: true
"#;
        let spec: SnapshotPolicySpec = from_yaml(yaml);
        assert_eq!(
            spec.credential_projection.as_ref().map(|p| p.enabled),
            Some(true)
        );
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["credentialProjection"]["enabled"], true);
        let reparsed: SnapshotPolicySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent ⇒ None (self-managed default); not serialized.
        let bare: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\n",
        );
        assert!(bare.credential_projection.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("credentialProjection")
                .is_none()
        );
        // Empty `{}` defaults enabled=false (opt-in).
        let empty: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\ncredentialProjection: {}\n",
        );
        assert_eq!(empty.credential_projection.map(|p| p.enabled), Some(false));
    }

    #[test]
    fn backup_config_minimal_selector_source() {
        // Mirrors ADR-0001 §5.4 (multi-PVC selector).
        let yaml = r#"
repository: { kind: Repository, name: nas-primary, namespace: backups }
identity: { username: app-bundle, hostname: billing }
sources:
  - pvcSelector:
      labelSelector: { matchLabels: { backup: include } }
    sourcePathStrategy: PvcName
groupBy: VolumeGroupSnapshot
retention: { keepDaily: 14 }
"#;
        let spec: SnapshotPolicySpec = from_yaml(yaml);
        let src = &spec.sources[0];
        assert!(src.pvc.is_none());
        assert!(src.pvc_selector.is_some());
        assert_eq!(src.source_path_strategy, Some(SourcePathStrategy::PvcName));

        let json = serde_json::to_value(&spec).unwrap();
        let reparsed: SnapshotPolicySpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn hook_run_job_variant_with_job_spec() {
        // RunJob embeds a full k8s-openapi JobSpec (so the struct is not Eq).
        let yaml = r#"
runJob:
  jobSpec:
    template:
      spec:
        restartPolicy: Never
        containers:
          - name: pre
            image: busybox
            command: ["sh", "-c", "echo hi"]
  timeout: 5m
  continueOnFailure: true
"#;
        let hook: Hook = from_yaml(yaml);
        assert_eq!(hook.kind_str(), "RunJob");
        match &hook {
            Hook::RunJob(j) => {
                assert!(j.continue_on_failure);
                assert_eq!(j.timeout.as_deref(), Some("5m"));
                assert_eq!(
                    j.job_spec
                        .template
                        .spec
                        .as_ref()
                        .unwrap()
                        .restart_policy
                        .as_deref(),
                    Some("Never")
                );
            }
            other => panic!("expected RunJob, got {}", other.kind_str()),
        }
        let json = serde_json::to_value(&hook).unwrap();
        assert!(json.get("runJob").is_some());
    }

    #[test]
    fn hook_http_request_variant() {
        let hook: Hook = from_yaml(
            "httpRequest:\n  url: https://example/notify\n  method: POST\n  headers:\n    - name: Content-Type\n      value: application/json\n    - name: X-Api-Key\n      value: sekrit\n",
        );
        assert_eq!(hook.kind_str(), "HttpRequest");
        let v = serde_json::to_value(&hook).unwrap();
        assert_eq!(v["httpRequest"]["url"], "https://example/notify");
        assert_eq!(
            v.pointer("/httpRequest/headers/0/name")
                .and_then(|x| x.as_str()),
            Some("Content-Type")
        );
        assert_eq!(
            v.pointer("/httpRequest/headers/1/value")
                .and_then(|x| x.as_str()),
            Some("sekrit")
        );
        // Omitted headers stay off the wire (skip_serializing_if).
        let bare: Hook = from_yaml("httpRequest:\n  url: https://example/notify\n");
        let v = serde_json::to_value(&bare).unwrap();
        assert!(v.pointer("/httpRequest/headers").is_none());
    }

    #[test]
    fn hook_unknown_variant_is_rejected() {
        let value: serde_json::Value = serde_yaml::from_str("teleport:\n  url: x\n").unwrap();
        assert!(serde_json::from_value::<Hook>(value).is_err());
    }

    #[test]
    fn error_handling_upload_and_suspend_roundtrip() {
        // ADR-0005 §13(b)/§13(f)/§14(e): the new policy knobs parse the cluster's
        // way, default sanely when absent, and round-trip.
        let yaml = r#"
repository: { kind: Repository, name: r }
sources: [ { pvc: { name: d } } ]
errorHandling:
  ignoreFileErrors: true
  ignoreDirErrors: false
  ignoreUnknownTypes: true
  failFast: true
upload:
  maxParallelSnapshots: 4
  maxParallelFileReads: 8
  limitMb: 100
suspend: true
"#;
        let spec: SnapshotPolicySpec = from_yaml(yaml);
        let eh = spec.error_handling.as_ref().expect("errorHandling");
        assert!(eh.ignore_file_errors);
        assert!(!eh.ignore_dir_errors);
        assert!(eh.ignore_unknown_types);
        assert!(eh.fail_fast);
        let up = spec.upload.as_ref().expect("upload");
        assert_eq!(up.max_parallel_snapshots, Some(4));
        assert_eq!(up.max_parallel_file_reads, Some(8));
        assert_eq!(up.limit_mb, Some(100));
        assert!(spec.suspend);

        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["suspend"], true);
        assert_eq!(json["errorHandling"]["ignoreFileErrors"], true);
        assert_eq!(json["errorHandling"]["failFast"], true);
        assert_eq!(json["upload"]["maxParallelSnapshots"], 4);
        assert_eq!(json["upload"]["limitMb"], 100);
        let reparsed: SnapshotPolicySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent ⇒ None / false (not serialized).
        let bare: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\n",
        );
        assert!(bare.error_handling.is_none());
        assert!(bare.upload.is_none());
        assert!(!bare.suspend);
        let bare_json = serde_json::to_value(&bare).unwrap();
        assert!(bare_json.get("suspend").is_none());
        assert!(bare_json.get("errorHandling").is_none());
    }

    #[test]
    fn source_schema_carries_exactly_one_of_validation() {
        // §15: the Source sub-object schema carries the exactly-one-of(pvc/
        // pvcSelector/nfs) rule, surviving kube's structural-schema rewriter even as a
        // list-item sub-object.
        let crd = SnapshotPolicy::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let source = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["sources"]["items"];
        let rules = source["x-kubernetes-validations"]
            .as_array()
            .expect("sources.items.x-kubernetes-validations present");
        assert!(rules.iter().any(|r| {
            r["rule"]
                .as_str()
                .is_some_and(|s| s.contains("pvcSelector") && s.contains("nfs"))
        }));
    }

    #[test]
    fn snapshot_policy_has_last_snapshot_and_suspended_columns() {
        // ADR-0005 §3: the LAST-SNAPSHOT (status.lastSuccessfulSnapshot) and
        // §14(e) SUSPENDED columns are present in the CRD with the right jsonPaths.
        let crd = SnapshotPolicy::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let cols = json["spec"]["versions"][0]["additionalPrinterColumns"]
            .as_array()
            .expect("printer columns");
        let by_name = |name: &str| {
            cols.iter()
                .find(|c| c["name"] == name)
                .unwrap_or_else(|| panic!("missing column {name}"))
        };
        assert_eq!(
            by_name("Last-Snapshot")["jsonPath"],
            ".status.lastSuccessfulSnapshot"
        );
        assert_eq!(by_name("Suspended")["jsonPath"], ".spec.suspend");
    }

    #[test]
    fn verification_roundtrip_and_opt_in() {
        // ADR-0005 §4: verification parses the cluster's way, round-trips, and is
        // opt-in (absent ⇒ None, no behavior change).
        let yaml = r#"
repository: { kind: Repository, name: r }
sources: [ { pvc: { name: d } } ]
verification:
  quick:
    schedule: { cron: "0 4 * * *", jitter: 30m }
  deep:
    schedule: { cron: "0 5 * * 0", jitter: 1h }
    capacity: 10Gi
    storageClassName: fast-ssd
  successExpr: "stats.files > 0 && stats.errors == 0"
  verifyFilesPercent: 10
"#;
        let spec: SnapshotPolicySpec = from_yaml(yaml);
        let v = spec.verification.as_ref().expect("verification");
        let quick = v.quick.as_ref().expect("quick");
        assert_eq!(quick.schedule.as_ref().unwrap().cron, "0 4 * * *");
        let deep = v.deep.as_ref().expect("deep");
        assert_eq!(deep.schedule.cron, "0 5 * * 0");
        assert_eq!(deep.capacity.as_deref(), Some("10Gi"));
        assert_eq!(
            v.success_expr.as_deref(),
            Some("stats.files > 0 && stats.errors == 0")
        );
        assert_eq!(v.verify_files_percent, Some(10));

        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(
            json["verification"]["quick"]["schedule"]["cron"],
            "0 4 * * *"
        );
        let reparsed: SnapshotPolicySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent ⇒ None (no behavior change).
        let bare: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\n",
        );
        assert!(bare.verification.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("verification")
                .is_none()
        );
    }

    #[test]
    fn verification_quick_old_shape_still_decodes() {
        // GitHub #174: `verification.quick` gained a nested `schedule`. An object
        // persisted in etcd BEFORE this change carries the flat shape
        // (`quick: { cron: ... }`). It MUST still decode (serde ignores the unknown
        // `cron`/`jitter` keys) as `schedule: None` — a hard decode failure would
        // wedge the SnapshotPolicy reflector and poison admission cluster-wide. The
        // quick tier is then treated as disabled; the webhook rejects NEW old-shape
        // writes with a pointer to the move.
        let old = from_yaml::<SnapshotPolicySpec>(
            "repository: { kind: Repository, name: r }\n\
             sources: [ { pvc: { name: d } } ]\n\
             verification:\n  quick: { cron: \"0 4 * * *\", jitter: 30m }\n",
        );
        let v = old.verification.as_ref().expect("verification");
        let quick = v.quick.as_ref().expect("quick present");
        assert!(
            quick.schedule.is_none(),
            "old flat `quick: {{cron: ...}}` must decode with schedule: None (quick disabled)"
        );
    }

    #[test]
    fn verification_quick_and_deep_tuning_knobs_roundtrip() {
        // M3 (issue #216 category sweep): quick gains `--parallel`/`--file-parallelism`/
        // `--file-queue-length`/`--max-errors`; deep gains `--parallel` (it restores
        // under the hood). All optional, absent ⇒ kopia's own default.
        let yaml = r#"
repository: { kind: Repository, name: r }
sources: [ { pvc: { name: d } } ]
verification:
  quick:
    schedule: { cron: "0 4 * * *" }
    parallel: 2
    fileParallelism: 4
    fileQueueLength: 100
    maxErrors: 1
  deep:
    schedule: { cron: "0 5 * * 0" }
    parallel: 2
"#;
        let spec: SnapshotPolicySpec = from_yaml(yaml);
        let v = spec.verification.as_ref().expect("verification");
        let quick = v.quick.as_ref().expect("quick");
        assert_eq!(quick.parallel, Some(2));
        assert_eq!(quick.file_parallelism, Some(4));
        assert_eq!(quick.file_queue_length, Some(100));
        assert_eq!(quick.max_errors, Some(1));
        let deep = v.deep.as_ref().expect("deep");
        assert_eq!(deep.parallel, Some(2));

        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["verification"]["quick"]["parallel"], 2);
        assert_eq!(json["verification"]["quick"]["fileParallelism"], 4);
        assert_eq!(json["verification"]["quick"]["fileQueueLength"], 100);
        assert_eq!(json["verification"]["quick"]["maxErrors"], 1);
        assert_eq!(json["verification"]["deep"]["parallel"], 2);
        let reparsed: SnapshotPolicySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent ⇒ None, and the keys are omitted entirely (no dormant defaults).
        let bare_yaml = r#"
repository: { kind: Repository, name: r }
sources: [ { pvc: { name: d } } ]
verification:
  quick:
    schedule: { cron: "0 4 * * *" }
  deep:
    schedule: { cron: "0 5 * * 0" }
"#;
        let bare: SnapshotPolicySpec = from_yaml(bare_yaml);
        let bv = bare.verification.as_ref().expect("verification");
        assert!(bv.quick.as_ref().unwrap().parallel.is_none());
        assert!(bv.deep.as_ref().unwrap().parallel.is_none());
        let bare_json = serde_json::to_value(&bare).expect("serialize");
        assert!(bare_json["verification"]["quick"].get("parallel").is_none());
        assert!(bare_json["verification"]["deep"].get("parallel").is_none());
    }

    #[test]
    fn preflight_roundtrip_and_opt_in() {
        // Preflight parses the cluster's way, round-trips, and is opt-in.
        let yaml = r#"
repository: { kind: Repository, name: r }
sources: [ { pvc: { name: d } } ]
preflight:
  timeout: 10m
  checks:
    - name: maintenance-fresh
      expr: "maintenance.hasRun && maintenance.lastSuccessAgeSeconds < 604800"
      message: "maintenance must have run within 7d"
    - name: backend-up
      expr: "repository.backendReachable"
"#;
        let spec: SnapshotPolicySpec = from_yaml(yaml);
        let pf = spec.preflight.as_ref().expect("preflight");
        assert_eq!(pf.timeout.as_deref(), Some("10m"));
        assert_eq!(pf.checks.len(), 2);
        assert_eq!(pf.checks[0].name, "maintenance-fresh");
        assert_eq!(pf.checks[1].expr, "repository.backendReachable");
        assert!(pf.checks[1].message.is_none());

        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["preflight"]["checks"][0]["name"], "maintenance-fresh");
        let reparsed: SnapshotPolicySpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent ⇒ None (no behavior change).
        let bare: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\n",
        );
        assert!(bare.preflight.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("preflight")
                .is_none()
        );
    }

    #[test]
    fn snapshot_policy_has_last_verified_column() {
        // ADR-0005 §4: the LAST-VERIFIED (status.lastVerified) column is present.
        let crd = SnapshotPolicy::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let cols = json["spec"]["versions"][0]["additionalPrinterColumns"]
            .as_array()
            .expect("printer columns");
        let col = cols
            .iter()
            .find(|c| c["name"] == "Last-Verified")
            .expect("Last-Verified column");
        assert_eq!(col["jsonPath"], ".status.lastVerified");
    }

    #[test]
    fn snapshot_policy_has_repositories_summary_column() {
        // #368 B1: the REPOSITORIES column renders status.repositorySummary so a
        // multi-repo policy (whose `.spec.repository.name` column is empty) still
        // names its targets in `kubectl get`.
        let crd = SnapshotPolicy::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let cols = json["spec"]["versions"][0]["additionalPrinterColumns"]
            .as_array()
            .expect("printer columns");
        let col = cols
            .iter()
            .find(|c| c["name"] == "Repositories")
            .expect("Repositories column");
        assert_eq!(col["jsonPath"], ".status.repositorySummary");
    }

    #[test]
    fn status_verification_fields_elide_when_empty_and_roundtrip() {
        // The golden-byte contract for every new Vec/map status field (#368):
        // an empty `verification` / `verificationStamps` / absent
        // `repositorySummary` must emit NOTHING, so a single-repo policy's
        // status wire is byte-identical to pre-feature operators.
        let bare = SnapshotPolicyStatus::default();
        let json = serde_json::to_value(&bare).expect("serialize");
        assert!(json.get("verification").is_none(), "empty vec must elide");
        assert!(
            json.get("verificationStamps").is_none(),
            "empty map must elide"
        );
        assert!(
            json.get("repositorySummary").is_none(),
            "absent summary must elide"
        );

        // Populated: parse the cluster's way and round-trip.
        let status: SnapshotPolicyStatus = serde_json::from_value(serde_json::json!({
            "lastVerified": "2026-08-01T00:00:00Z",
            "repositorySummary": "nas, offsite",
            "verification": [
                {
                    "repository": { "kind": "Repository", "name": "nas", "namespace": "backups" },
                    "lastVerified": "2026-08-01T00:00:00Z"
                },
                { "repository": { "kind": "ClusterRepository", "name": "offsite" } }
            ],
            "verificationStamps": {
                "Repository/backups/nas": "2026-08-01T00:00:00Z"
            }
        }))
        .expect("status parses");
        assert_eq!(status.verification.len(), 2);
        assert_eq!(
            status.verification[0].last_verified.as_deref(),
            Some("2026-08-01T00:00:00Z")
        );
        assert!(status.verification[1].last_verified.is_none());
        assert_eq!(
            status
                .verification_stamps
                .get("Repository/backups/nas")
                .map(String::as_str),
            Some("2026-08-01T00:00:00Z")
        );
        let out = serde_json::to_value(&status).expect("serialize");
        assert_eq!(
            out["verification"][0]["repository"]["name"], "nas",
            "camelCase wire shape"
        );
        assert_eq!(out["repositorySummary"], "nas, offsite");
    }

    #[test]
    fn status_last_successful_snapshot_roundtrips() {
        let status: SnapshotPolicyStatus =
            from_yaml("lastSuccessfulSnapshot: 2026-06-09T02:00:00Z\n");
        assert_eq!(
            status.last_successful_snapshot.as_deref(),
            Some("2026-06-09T02:00:00Z")
        );
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["lastSuccessfulSnapshot"], "2026-06-09T02:00:00Z");
    }

    // --- policy-deletion cascade (spec.deletion.onPolicyDelete) --------------

    #[test]
    fn policy_deletion_on_policy_delete_schema_default_is_retain() {
        // Mirrors snapshot_schedule's schedule_deletion_on_schedule_delete_schema_default_is_retain:
        // context-free default, safe to server-side-materialize because
        // effective_on_policy_delete maps an absent sub-object to the same value.
        let crd = SnapshotPolicy::crd();
        let json = serde_json::to_value(&crd).unwrap();
        let spec = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"];
        assert_eq!(
            spec["properties"]["deletion"]["properties"]["onPolicyDelete"]["default"],
            serde_json::json!("Retain")
        );
        assert_eq!(
            effective_on_policy_delete(None),
            crate::common::PolicyDeletePolicy::Retain
        );
    }

    #[test]
    fn policy_deletion_round_trips_and_absent_stays_none() {
        use crate::common::PolicyDeletePolicy;

        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\n\
             sources: [ { pvc: { name: d } } ]\n\
             deletion: { onPolicyDelete: Delete }\n",
        );
        assert_eq!(
            spec.deletion.as_ref().map(|d| d.on_policy_delete),
            Some(PolicyDeletePolicy::Delete)
        );
        assert_eq!(
            effective_on_policy_delete(spec.deletion.as_ref()),
            PolicyDeletePolicy::Delete
        );
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["deletion"]["onPolicyDelete"], "Delete");
        let reparsed: SnapshotPolicySpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec, reparsed);

        // Absent sub-object stays None (not materialized to Retain client-side).
        let bare: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\n",
        );
        assert!(bare.deletion.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("deletion")
                .is_none(),
            "absent deletion must be elided"
        );
        assert_eq!(
            effective_on_policy_delete(bare.deletion.as_ref()),
            PolicyDeletePolicy::Retain
        );
    }

    #[test]
    fn policy_deletion_unknown_on_policy_delete_value_is_rejected() {
        let value: serde_json::Value =
            serde_yaml::from_str("deletion:\n  onPolicyDelete: Orphan\n").unwrap();
        assert!(serde_json::from_value::<SnapshotPolicySpec>(value).is_err());
    }

    #[test]
    fn policy_delete_policy_serializes_to_expected_strings() {
        use crate::common::PolicyDeletePolicy;

        assert_eq!(
            serde_json::to_value(PolicyDeletePolicy::Retain).unwrap(),
            "Retain"
        );
        assert_eq!(
            serde_json::to_value(PolicyDeletePolicy::Delete).unwrap(),
            "Delete"
        );
        assert_eq!(PolicyDeletePolicy::default(), PolicyDeletePolicy::Retain);
    }

    // --- adoption (spec.adoption + status.adoption) --------------------------

    #[test]
    fn policy_adoption_round_trips_and_absent_stays_none() {
        use crate::common::SnapshotAdoption;

        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\n\
             sources: [ { pvc: { name: d } } ]\n\
             adoption: Ignore\n",
        );
        assert_eq!(spec.adoption, Some(SnapshotAdoption::Ignore));
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["adoption"], "Ignore");
        let reparsed: SnapshotPolicySpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec, reparsed);

        let bare: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\n",
        );
        assert!(bare.adoption.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("adoption")
                .is_none(),
            "absent adoption must be elided"
        );
    }

    #[test]
    fn policy_adoption_schema_carries_no_default() {
        // §4a: the effective default (`Adopt`) is context-dependent (a policy ->
        // repo -> constant inheritance chain), so no schemars `default` is
        // emitted for THIS field — the reference stays `—`.
        let crd = SnapshotPolicy::crd();
        let json = serde_json::to_value(&crd).unwrap();
        let prop = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["adoption"];
        assert!(
            prop.get("default").is_none(),
            "spec.adoption must NOT carry a schema default: {prop}"
        );
        assert_eq!(prop["enum"].as_array().map(|a| a.len()), Some(2), "{prop}");
    }

    // --- M7 multi-repository fan-out: golden byte-compat + accessors ---------

    /// THE golden byte-compat proof for the `repository: Option` flip: a legacy
    /// single-repo spec's wire encoding is pinned as a COMMITTED fixture string
    /// (the pre-change encoding), not constructed by round-tripping — so any
    /// serialization drift (a leaked `repositories` key above all) fails here.
    #[test]
    fn legacy_single_repo_spec_wire_is_byte_identical() {
        const LEGACY_WIRE: &str = r#"{"repository":{"kind":"Repository","name":"nas-primary","namespace":"backups"},"sources":[{"pvc":{"name":"pgdata"}}],"copyMethod":"Snapshot","retention":{"keepLatest":3}}"#;
        assert!(
            !LEGACY_WIRE.contains("repositories"),
            "fixture precondition"
        );

        // A legacy YAML parses to the same struct shape it always did…
        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: nas-primary, namespace: backups }\n\
             sources: [ { pvc: { name: pgdata } } ]\n\
             retention: { keepLatest: 3 }\n",
        );
        assert_eq!(spec.repository.as_ref().unwrap().name, "nas-primary");
        assert!(spec.repositories.is_empty());

        // …and re-serializes to the exact pre-change bytes: no `repositories`
        // key (Vec::is_empty skip), `repository` unwrapped exactly as before.
        assert_eq!(serde_json::to_string(&spec).unwrap(), LEGACY_WIRE);

        // The committed wire decodes back to the identical struct.
        let reparsed: SnapshotPolicySpec = serde_json::from_str(LEGACY_WIRE).unwrap();
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn repositories_roundtrip_and_absent_stays_off_the_wire() {
        let spec: SnapshotPolicySpec = from_yaml(
            "repositories:\n\
             - { kind: Repository, name: a }\n\
             - { kind: ClusterRepository, name: b }\n\
             sources: [ { pvc: { name: d } } ]\n",
        );
        assert!(spec.repository.is_none());
        assert_eq!(spec.repositories.len(), 2);
        let json = serde_json::to_value(&spec).unwrap();
        assert!(json.get("repository").is_none());
        assert_eq!(json["repositories"][1]["kind"], "ClusterRepository");
        let reparsed: SnapshotPolicySpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn policy_repositories_accessors_cover_all_four_shapes() {
        use crate::error::ValidationError;

        let single: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\n",
        );
        assert!(matches!(
            policy_repositories(&single),
            Ok(PolicyRepositories::Single(r)) if r.name == "r"
        ));
        assert!(!is_multi_repo(&single));
        assert_eq!(repository_refs(&single).count(), 1);
        assert_eq!(single_repository_ref(&single).unwrap().name, "r");

        let multi: SnapshotPolicySpec = from_yaml(
            "repositories: [ { name: a }, { name: b } ]\nsources: [ { pvc: { name: d } } ]\n",
        );
        assert!(matches!(
            policy_repositories(&multi),
            Ok(PolicyRepositories::Multi(refs)) if refs.len() == 2
        ));
        assert!(is_multi_repo(&multi));
        assert_eq!(
            repository_refs(&multi)
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        // The single accessor refuses multi with the feature-gate variant —
        // never a silent repository #1.
        assert_eq!(
            single_repository_ref(&multi).unwrap_err(),
            ValidationError::MultiRepositoryNotYetEnabled
        );

        // Neither / both: named errors, never a panic — a stored CR can be
        // garbage relative to the current CRD schema.
        let neither: SnapshotPolicySpec = from_yaml("sources: [ { pvc: { name: d } } ]\n");
        assert_eq!(
            policy_repositories(&neither).unwrap_err(),
            ValidationError::PolicyRepositoryExactlyOne { got: "neither" }
        );
        assert_eq!(repository_refs(&neither).count(), 0);

        let both: SnapshotPolicySpec = from_yaml(
            "repository: { name: r }\nrepositories: [ { name: a } ]\n\
             sources: [ { pvc: { name: d } } ]\n",
        );
        assert_eq!(
            policy_repositories(&both).unwrap_err(),
            ValidationError::PolicyRepositoryExactlyOne { got: "both" }
        );
        // Tolerant iterator yields whatever exists, in repository-then-list order.
        assert_eq!(
            repository_refs(&both)
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["r", "a"]
        );
    }

    #[test]
    fn select_restore_repository_covers_every_selection_shape() {
        use super::select_restore_repository;
        use crate::common::{RepositoryKind, RepositoryRef};
        use crate::error::ValidationError;
        let single: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\n",
        );
        let multi: SnapshotPolicySpec = from_yaml(
            "repositories:\n  - { kind: Repository, name: a }\n  - { kind: ClusterRepository, name: b }\n\
             sources: [ { pvc: { name: d } } ]\n",
        );
        let rref = |kind, name: &str, ns: Option<&str>| RepositoryRef {
            kind,
            name: name.into(),
            namespace: ns.map(str::to_string),
        };

        // No selection, single-repo → the policy's one ref, verbatim.
        let r = select_restore_repository(&single, "pol", "backups", None, "apps").unwrap();
        assert_eq!(r.name, "r");

        // No selection, multi-repo → refusal naming every valid choice.
        match select_restore_repository(&multi, "pol", "backups", None, "apps").unwrap_err() {
            ValidationError::RestoreRepositorySelectionRequired { policy, valid } => {
                assert_eq!(policy, "pol");
                assert_eq!(valid, "Repository/backups/a, ClusterRepository/b");
            }
            other => panic!("expected RestoreRepositorySelectionRequired, got {other:?}"),
        }

        // Explicit member (namespace-qualified to the policy's namespace) → honored.
        let explicit = rref(RepositoryKind::Repository, "a", Some("backups"));
        let r =
            select_restore_repository(&multi, "pol", "backups", Some(&explicit), "apps").unwrap();
        assert_eq!(r.name, "a");

        // Explicit cluster-scoped member: namespace-free key matches from any
        // restore namespace.
        let cluster = rref(RepositoryKind::ClusterRepository, "b", None);
        assert!(
            select_restore_repository(&multi, "pol", "backups", Some(&cluster), "apps").is_ok()
        );

        // Explicit NON-member → typed refusal naming what was given and what's
        // valid. (An unqualified Repository ref resolves in the RESTORE's
        // namespace — `apps/a` is not the policy's `backups/a`.)
        let typo = rref(RepositoryKind::Repository, "a", None);
        match select_restore_repository(&multi, "pol", "backups", Some(&typo), "apps").unwrap_err()
        {
            ValidationError::RestoreRepositoryNotInPolicy {
                given,
                policy,
                valid,
            } => {
                assert_eq!(given, "Repository/apps/a");
                assert_eq!(policy, "pol");
                assert_eq!(valid, "Repository/backups/a, ClusterRepository/b");
            }
            other => panic!("expected RestoreRepositoryNotInPolicy, got {other:?}"),
        }

        // Explicit non-member against a SINGLE-repo policy is refused too (the
        // audit-m4 backstop: a typo must not silently read the wrong repo).
        let elsewhere = rref(RepositoryKind::ClusterRepository, "offsite", None);
        assert!(matches!(
            select_restore_repository(&single, "pol", "backups", Some(&elsewhere), "apps")
                .unwrap_err(),
            ValidationError::RestoreRepositoryNotInPolicy { .. }
        ));
    }

    #[test]
    fn snapshot_policy_spec_carries_exactly_one_of_repository_cel_rule() {
        // The spec-level CEL rule must survive kube's structural-schema
        // rewriter (mirrors restore.rs / snapshot_schedule.rs precedents).
        let crd = SnapshotPolicy::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let spec = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"];
        let rules = spec["x-kubernetes-validations"]
            .as_array()
            .expect("spec-level x-kubernetes-validations present");
        assert!(
            rules.iter().any(|r| {
                r["rule"]
                    == "(has(self.repository) ? 1 : 0) + (has(self.repositories) ? 1 : 0) == 1"
                    && r["message"] == "exactly one of repository, repositories"
            }),
            "missing the exactly-one-of CEL rule; got {rules:?}"
        );
        // `repository` is no longer structurally required…
        let required = spec["required"].as_array().cloned().unwrap_or_default();
        assert!(
            !required.iter().any(|f| f == "repository"),
            "repository must not be schema-required anymore; got {required:?}"
        );
        // …and `repositories` carries the 1..=8 bounds (minItems 1 makes an
        // explicit `repositories: []` a structural rejection, so the CEL rule
        // never has to reason about a present-but-empty list).
        let repos = &spec["properties"]["repositories"];
        assert_eq!(repos["minItems"], 1, "{repos}");
        assert_eq!(repos["maxItems"], 8, "{repos}");
    }

    #[test]
    fn snapshot_policy_repository_print_column_is_unchanged() {
        // The `Repository` column stays `.spec.repository.name` (renders empty
        // for a future multi-repo policy; documented when the gate lifts).
        let crd = SnapshotPolicy::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let cols = json["spec"]["versions"][0]["additionalPrinterColumns"]
            .as_array()
            .expect("printer columns");
        let col = cols
            .iter()
            .find(|c| c["name"] == "Repository")
            .expect("Repository column");
        assert_eq!(col["jsonPath"], ".spec.repository.name");
    }

    #[test]
    fn adoption_summary_status_roundtrips() {
        let status: SnapshotPolicyStatus = from_yaml(
            "adoption:\n  \
             lastAdoptionAt: 2026-06-09T02:00:00Z\n  \
             lastAdoptedCount: 3\n  \
             totalAdopted: 42\n  \
             scanRequestedAt: 2026-06-10T00:00:00Z\n  \
             scanRequestedIdentity: postgres@billing\n",
        );
        let a = status.adoption.as_ref().expect("adoption");
        assert_eq!(a.last_adoption_at.as_deref(), Some("2026-06-09T02:00:00Z"));
        assert_eq!(a.last_adopted_count, Some(3));
        assert_eq!(a.total_adopted, Some(42));
        assert_eq!(a.scan_requested_at.as_deref(), Some("2026-06-10T00:00:00Z"));
        assert_eq!(
            a.scan_requested_identity.as_deref(),
            Some("postgres@billing")
        );

        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["adoption"]["lastAdoptedCount"], 3);
        assert_eq!(json["adoption"]["totalAdopted"], 42);
        let reparsed: SnapshotPolicyStatus = serde_json::from_value(json).unwrap();
        assert_eq!(status, reparsed);

        // Absent ⇒ None, elided.
        let bare: SnapshotPolicyStatus = from_yaml("{}\n");
        assert!(bare.adoption.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("adoption")
                .is_none(),
            "absent adoption summary must be elided"
        );
    }
}
