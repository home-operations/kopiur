//! The `SnapshotPolicy` CRD — the *recipe*. Idempotent; runs nothing on its own.
//! ADR-0001 §3.3, ADR-0003 §4.8.

use crate::backend::NfsVolume;
use crate::common::{
    CredentialProjection, CronSpec, DeletionPolicy, Identity, MoverSpec, PodSelector,
    RepositoryRef, ResolvedIdentity, Retention,
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
    printcolumn = r#"{"name":"Last-Snapshot","type":"date","jsonPath":".status.lastSuccessfulSnapshot"}"#,
    printcolumn = r#"{"name":"Last-Verified","type":"date","jsonPath":".status.lastVerified"}"#,
    printcolumn = r#"{"name":"Suspended","type":"boolean","jsonPath":".spec.suspend"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPolicySpec {
    /// Discriminated reference to a `Repository` or `ClusterRepository`.
    pub repository: RepositoryRef,
    /// Identity overrides — what kopia records as `username@hostname:path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
    /// What to back up (at least one source; webhook-enforced).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 100))]
    pub sources: Vec<Source>,
    /// How the source volume is captured before kopia reads it: `Direct` (default), `Snapshot`, or `Clone`.
    #[serde(default = "default_copy_method")]
    #[schemars(default = "default_copy_method")]
    pub copy_method: CopyMethod,
    /// `VolumeSnapshotClass` used when `copyMethod` snapshots/clones the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_snapshot_class_name: Option<String>,
    /// Multi-PVC consistency grouping; `None` opts into independent per-PVC snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<GroupBy>,
    /// GFS retention, enforced by the operator pruning `Snapshot` CRs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<Retention>,
    /// Default `deletionPolicy` for `Snapshot` CRs created against this config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
}

/// A single backup source; exactly one of `pvc`, `pvcSelector`, `nfs` (webhook-enforced).
// The exactly-one-of rule is written as an integer sum of `has()` ternaries rather
// than `[...].filter(x,x).size()==1`: the apiserver estimates per-item CEL cost ×
// `maxItems`, and a list-construction + lambda `filter` blows the budget on the
// repeating `sources` list. The sum form is a cheap constant per item.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
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
    /// An inline NFS export to back up directly, mounted read-only. Mutually exclusive with `pvc`/`pvcSelector`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nfs: Option<NfsVolume>,
    /// What kopia records as the source path (default `/pvc/<name>`, or the NFS export `path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub source_path_override: Option<String>,
    /// How a `pvcSelector`-matched PVC's source path is derived (`pvcName` vs `pvcNamespacedName`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

/// serde/schemars `default` for [`SnapshotPolicySpec::copy_method`] — **`Direct`**.
///
/// `Direct` (read the live PVC) is the default for **backward compatibility and
/// portability**: it is the behavior that was actually in effect before `copyMethod`
/// was wired (the field was inert), and it works on **any** storage — no CSI snapshot
/// stack required. `Snapshot`/`Clone` (point-in-time CSI capture) are an explicit
/// opt-in for users who have the snapshot stack and want app-decoupled, point-in-time
/// backups. (Originally ADR-0005 §1 proposed `Snapshot` as the default; defaulting to it
/// would silently break every existing policy / non-CSI source on upgrade, so the
/// implemented default is `Direct`.)
///
/// A named fn so it backs BOTH `#[serde(default = ...)]` and `#[schemars(default = ...)]`,
/// which is what makes schemars 1 emit a real OpenAPI `default:` in the generated CRD.
fn default_copy_method() -> CopyMethod {
    CopyMethod::Direct
}

/// Volume snapshot copy method. Closed enum. ADR §3.3.
///
/// ```
/// use kopiur_api::CopyMethod;
///
/// // Defaults to a live read (Direct) — works on any storage, no CSI snapshot stack.
/// assert_eq!(CopyMethod::default(), CopyMethod::Direct);
/// // Serializes as a bare PascalCase string (no external tagging — it has no payload).
/// assert_eq!(serde_json::to_value(CopyMethod::Snapshot).unwrap(), "Snapshot");
/// assert_eq!(serde_json::to_value(CopyMethod::Direct).unwrap(), "Direct");
/// ```
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum CopyMethod {
    /// Point-in-time CSI volume snapshot (opt-in; requires the CSI snapshot stack + a `VolumeSnapshotClass`).
    Snapshot,
    /// CSI volume clone of the source, mounted read-only (opt-in; requires a cloning-capable CSI driver).
    Clone,
    /// Read the live PVC directly with no intermediate snapshot/clone (the default; works on any storage).
    #[default]
    Direct,
}

/// Multi-PVC grouping strategy. Closed enum. ADR §4.9.
///
/// Defaults to a consistent group snapshot; `None` must be set *explicitly* to
/// accept independent per-PVC snapshots, because a silent per-PVC fallback would
/// produce inconsistent backups (the data-integrity hazard ADR §4.9 guards against).
///
/// ```
/// use kopiur_api::GroupBy;
///
/// assert_eq!(GroupBy::default(), GroupBy::VolumeGroupSnapshot);
/// assert_eq!(serde_json::to_value(GroupBy::None).unwrap(), "None");
/// ```
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum GroupBy {
    /// Consistent group snapshot across all PVCs (default for multi-PVC).
    #[default]
    VolumeGroupSnapshot,
    /// Opt into independent per-PVC snapshots.
    None,
}

/// How a selector-matched PVC's source path is derived. Closed enum. ADR §3.3/§4.2.
///
/// Only relevant for `pvcSelector` sources, where one recipe expands to many PVCs
/// and each needs a distinct kopia source path.
///
/// ```
/// use kopiur_api::SourcePathStrategy;
///
/// assert_eq!(SourcePathStrategy::default(), SourcePathStrategy::PvcName);
/// assert_eq!(
///     serde_json::to_value(SourcePathStrategy::PvcNamespacedName).unwrap(),
///     "PvcNamespacedName"
/// );
/// ```
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum SourcePathStrategy {
    /// Path derived from the PVC name alone (default).
    #[default]
    PvcName,
    /// Path derived from `<namespace>/<name>` to disambiguate same-named PVCs across namespaces.
    PvcNamespacedName,
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
    /// Filename/path globs to exclude from the snapshot (e.g. `*.tmp`, `*/cache/*`, `lost+found`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
}

/// First-class backup verification proving snapshots are restorable; opt-in, with quick and deep tiers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Verification {
    /// Schedule for the frequent blob-level `kopia snapshot verify`; absent ⇒ no quick verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick: Option<CronSpec>,
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

/// One of three hook forms. ADR §4.8.
///
/// Externally-tagged: wire shape is `{ workloadExec: {...} }`, `{ runJob: {...} }`,
/// or `{ httpRequest: {...} }`. Exactly one variant by construction.
///
/// Not `Eq`: `RunJob` embeds `JobSpec` (k8s-openapi, `PartialEq` only).
///
/// ```
/// use kopiur_api::snapshot_policy::{Hook, HttpRequestHook};
///
/// // Construct directly — the type system guarantees exactly one variant.
/// let hook = Hook::HttpRequest(HttpRequestHook {
///     url: "https://example/notify".into(),
///     method: Some("POST".into()),
///     body: None,
///     timeout: None,
///     continue_on_failure: false,
/// });
/// assert_eq!(hook.kind_str(), "HttpRequest");
///
/// // Externally tagged on the wire: `{ httpRequest: { url: ... } }`.
/// let json = serde_json::to_value(&hook).unwrap();
/// assert_eq!(json["httpRequest"]["url"], "https://example/notify");
/// ```
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
    /// RFC3339 timestamp of the most recent successful child `Snapshot` from this recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_snapshot: Option<String>,
    /// RFC3339 timestamp of the most recent successful verification (any tier).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified: Option<String>,
    /// Standard Kubernetes conditions (e.g. `RepositoryReachable`, `GroupSnapshotSupported`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::RepositoryKind;
    use crate::testutil::from_yaml;
    use kube::core::CustomResourceExt;

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
        // copyMethod must carry a real schema `default: Direct` so it appears in
        // `kubectl explain` / the stored object and GitOps stops thrashing. `Direct` (not
        // the ADR-0005 §1 `Snapshot`) so wiring the field doesn't silently break every
        // existing policy / non-CSI source on upgrade — Snapshot/Clone are opt-in.
        let crd = SnapshotPolicy::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let default = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["copyMethod"]["default"];
        assert_eq!(
            default, "Direct",
            "copyMethod must emit `default: Direct` in the CRD schema; got {default:?}"
        );
    }

    #[test]
    fn copy_method_defaults_to_direct_when_absent() {
        // A bare value with a serde default: an omitted copyMethod parses to Direct (the
        // portable, backward-compatible live-mount behavior).
        let spec: SnapshotPolicySpec = from_yaml(
            "repository: { kind: Repository, name: r }\nsources: [ { pvc: { name: d } } ]\n",
        );
        assert_eq!(spec.copy_method, CopyMethod::Direct);
        // And it serializes (not skip-elided), so the materialized value round-trips.
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["copyMethod"], "Direct");
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
        assert_eq!(spec.repository.kind, RepositoryKind::Repository);
        assert_eq!(spec.repository.name, "nas-primary");
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
        let hook: Hook = from_yaml("httpRequest:\n  url: https://example/notify\n  method: POST\n");
        assert_eq!(hook.kind_str(), "HttpRequest");
        let json = serde_json::to_value(&hook).unwrap();
        assert_eq!(json["httpRequest"]["url"], "https://example/notify");
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
upload:
  maxParallelSnapshots: 4
  maxParallelFileReads: 8
suspend: true
"#;
        let spec: SnapshotPolicySpec = from_yaml(yaml);
        let eh = spec.error_handling.as_ref().expect("errorHandling");
        assert!(eh.ignore_file_errors);
        assert!(!eh.ignore_dir_errors);
        assert!(eh.ignore_unknown_types);
        let up = spec.upload.as_ref().expect("upload");
        assert_eq!(up.max_parallel_snapshots, Some(4));
        assert_eq!(up.max_parallel_file_reads, Some(8));
        assert!(spec.suspend);

        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["suspend"], true);
        assert_eq!(json["errorHandling"]["ignoreFileErrors"], true);
        assert_eq!(json["upload"]["maxParallelSnapshots"], 4);
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
  quick: { cron: "0 4 * * *", jitter: 30m }
  deep:
    schedule: { cron: "0 5 * * 0", jitter: 1h }
    capacity: 10Gi
    storageClassName: fast-ssd
  successExpr: "stats.files > 0 && stats.errors == 0"
  verifyFilesPercent: 10
"#;
        let spec: SnapshotPolicySpec = from_yaml(yaml);
        let v = spec.verification.as_ref().expect("verification");
        assert_eq!(v.quick.as_ref().unwrap().cron, "0 4 * * *");
        let deep = v.deep.as_ref().expect("deep");
        assert_eq!(deep.schedule.cron, "0 5 * * 0");
        assert_eq!(deep.capacity.as_deref(), Some("10Gi"));
        assert_eq!(
            v.success_expr.as_deref(),
            Some("stats.files > 0 && stats.errors == 0")
        );
        assert_eq!(v.verify_files_percent, Some(10));

        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["verification"]["quick"]["cron"], "0 4 * * *");
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
}
