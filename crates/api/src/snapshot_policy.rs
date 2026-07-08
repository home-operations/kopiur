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
    /// How the source volume is captured before kopia reads it: `Snapshot` (default), `Direct`, or `Clone`.
    #[serde(default = "default_copy_method")]
    #[schemars(default = "default_copy_method")]
    pub copy_method: CopyMethod,
    /// `VolumeSnapshotClass` used when `copyMethod` snapshots/clones the source.
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
    /// CSI volume clone of the source, mounted read-only (opt-in; requires a cloning-capable CSI driver).
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
    /// How long the staged `VolumeSnapshot` may take to become `readyToUse`
    /// (measured from its creation) before the backup is failed (Go-style
    /// duration like `10m` or `1h`; default `10m`). A transient CSI/
    /// snapshot-controller error during the wait is retried, never fatal on its
    /// own — only this deadline fails staging. A zero duration (`0`/`0s`) waits
    /// indefinitely. Raise this for backends whose snapshots take long to become
    /// ready (e.g. cloud snapshots of large volumes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
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
                timeout: Some("30m".to_string())
            })
        );
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["staging"]["timeout"], "30m");
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
