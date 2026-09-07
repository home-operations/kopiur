//! The `Restore` CRD — a restore from a snapshot/identity to a PVC, or a passive
//! populator source. ADR-0001 §3.6, ADR-0003 §4.6.

use crate::common::{
    CredentialProjection, FailurePolicy, MoverSpec, ObjectRef, PvcAccessMode, RepositoryRef,
    ResolvedIdentity,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A restore operation from a snapshot/identity into a PVC, or a passive populator source.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "kopiur.home-operations.com",
    version = "v1alpha1",
    kind = "Restore",
    namespaced,
    status = "RestoreStatus",
    shortname = "kopiarestore",
    category = "kopiur",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Source","type":"string","jsonPath":".status.sourceKind"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
// §15: operator-authored CEL in the CRD schema — exactly one of
// target.pvc/target.pvcRef/target.populator. Validates in the apiserver + CI
// (`kubeconform`), complementing the webhook. `target` is required so `has(self.target)`
// is always true; the rule counts the present sub-keys.
#[schemars(extend("x-kubernetes-validations" = [{
    "rule": "[has(self.target.pvc), has(self.target.pvcRef), has(self.target.populator)].filter(x, x).size() == 1",
    "message": "exactly one of target.pvc, target.pvcRef, target.populator"
}]))]
#[serde(rename_all = "camelCase")]
/// Desired state of a Restore: where to read from, where to write to, and how to behave when the snapshot is missing.
pub struct RestoreSpec {
    /// The repository to read from; derived from `source` when omitted, required only with `source.identity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryRef>,
    /// Where to read data from (snapshotRef, fromPolicy, or identity).
    pub source: RestoreSource,
    /// Where to write the restored data (pvc, pvcRef, or populator).
    pub target: RestoreTarget,
    /// kopia restore behavior (file deletion, permission/atomicity handling).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<RestoreOptions>,
    /// What to do when the referenced snapshot doesn't exist yet, and how long to wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<RestorePolicy>,
    /// Opt-in copying of the repository's credential Secret(s) into the mover's namespace (default off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_projection: Option<CredentialProjection>,
    /// Per-run mover overrides for this restore's Job (resources, cache, `securityContext`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mover: Option<MoverSpec>,
    /// Mover `Job` retry/deadline limits (`backoffLimit`, `activeDeadlineSeconds`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_policy: Option<FailurePolicy>,
}

/// Where to restore from; exactly one variant.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RestoreSource {
    /// A `Snapshot` CR (scheduled, manual, or discovered).
    SnapshotRef(ObjectRef),
    /// A `SnapshotPolicy` CR, resolved via identity even with no `Snapshot` CR present (deploy-or-restore).
    FromPolicy(FromPolicy),
    /// A raw kopia identity (foreign writers / aged-out catalog); requires `spec.repository`.
    Identity(IdentitySource),
}

impl RestoreSource {
    /// Stable discriminant string for status/metrics.
    ///
    /// ```
    /// use kopiur_api::common::ObjectRef;
    /// use kopiur_api::restore::RestoreSource;
    ///
    /// let src = RestoreSource::SnapshotRef(ObjectRef { name: "pg-20260524".into(), namespace: None });
    /// assert_eq!(src.kind_str(), "SnapshotRef");
    ///
    /// // Externally tagged: each variant deserializes under its own camelCase key.
    /// let from_cfg: RestoreSource =
    ///     serde_json::from_value(serde_json::json!({ "fromPolicy": { "name": "pg" } })).unwrap();
    /// assert_eq!(from_cfg.kind_str(), "FromPolicy");
    /// ```
    pub fn kind_str(&self) -> &'static str {
        match self {
            RestoreSource::SnapshotRef(_) => "SnapshotRef",
            RestoreSource::FromPolicy(_) => "FromPolicy",
            RestoreSource::Identity(_) => "Identity",
        }
    }
}

/// The `fromPolicy` source: resolve a snapshot via a `SnapshotPolicy`'s identity, even when no `Snapshot` CR exists yet.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FromPolicy {
    /// Name of the `SnapshotPolicy` whose identity selects the snapshot.
    pub name: String,
    /// Namespace of the `SnapshotPolicy`; absent = the `Restore`'s own namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Restore the newest snapshot at or before this RFC3339 timestamp (point-in-time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
    /// Which snapshot to pick: 0 = latest, 1 = previous, and so on.
    #[serde(default = "default_offset")]
    #[schemars(default = "default_offset")]
    pub offset: i64,
    /// The kopia source path to restore FROM, overriding the path kopiur derives
    /// from the policy.
    ///
    /// Normally the path is derived: a policy with one plain `pvc:`/`nfs` source
    /// contributes its own path, and a `pvcSelector` policy contributes the path
    /// its `sourcePathStrategy` would have produced for the PVC being restored
    /// (`/pvc/<name>` or `/pvc/<namespace>/<name>`) — the same rule the backup
    /// side used when it wrote the snapshot, so a fan-out restore fills each PVC
    /// from ITS OWN snapshot instead of the newest snapshot of any member.
    ///
    /// Set this when the derivation is ambiguous or wrong: selector sources that
    /// disagree on `sourcePathStrategy`/`sourcePathOverride` (kopiur fails closed
    /// rather than guess), a policy whose selector sources share one
    /// `sourcePathOverride`, or a cross-namespace `target.pvcRef` whose derived
    /// `/pvc/<namespace>/<name>` names a namespace the repository never saw.
    ///
    /// Mirrors `IdentitySource::source_path`: it selects which kopia source to
    /// READ, it does not change where the data is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub source_path: Option<String>,
}

/// serde/schemars `default` for [`FromPolicy::offset`] — `0`, the latest snapshot
/// (ADR-0005 §1). A named fn so it backs BOTH `#[serde(default = ...)]` and
/// `#[schemars(default = ...)]`, which is what makes schemars 1 emit the OpenAPI
/// `default:` in the generated CRD schema.
fn default_offset() -> i64 {
    0
}

/// The `identity` source: a raw kopia `username@hostname:path` identity; requires `spec.repository`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IdentitySource {
    /// The kopia `username` to match.
    pub username: String,
    /// The kopia `hostname` to match.
    pub hostname: String,
    /// The kopia source path to match; absent matches any path for the identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// Pin an exact kopia snapshot by ID.
    #[serde(
        default,
        rename = "snapshotID",
        skip_serializing_if = "Option::is_none"
    )]
    pub snapshot_id: Option<String>,
    /// Restore the newest snapshot at or before this RFC3339 timestamp (point-in-time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
    /// Which snapshot to pick: 0 = latest, 1 = previous, and so on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
}

/// Where to restore to; exactly one variant.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RestoreTarget {
    /// Operator creates the PVC.
    Pvc(PvcTemplate),
    /// Write into an existing PVC.
    PvcRef(ObjectRef),
    /// Passive populator mode: the restore is claimed by a PVC's `spec.dataSourceRef`.
    Populator(PopulatorTarget),
}

impl RestoreTarget {
    /// Stable discriminant string for status/metrics.
    ///
    /// ```
    /// use kopiur_api::common::ObjectRef;
    /// use kopiur_api::restore::RestoreTarget;
    ///
    /// let into_existing = RestoreTarget::PvcRef(ObjectRef { name: "data".into(), namespace: None });
    /// assert_eq!(into_existing.kind_str(), "PvcRef");
    ///
    /// // Externally tagged: `{ pvc: {...} }` selects the create-PVC variant.
    /// let created: RestoreTarget =
    ///     serde_json::from_value(serde_json::json!({ "pvc": { "name": "restored" } })).unwrap();
    /// assert_eq!(created.kind_str(), "Pvc");
    /// ```
    pub fn kind_str(&self) -> &'static str {
        match self {
            RestoreTarget::Pvc(_) => "Pvc",
            RestoreTarget::PvcRef(_) => "PvcRef",
            RestoreTarget::Populator(_) => "Populator",
        }
    }
}

/// Passive-populator target marker; its presence selects populator mode.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PopulatorTarget {}

/// Template for a PVC the operator creates as the restore target.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PvcTemplate {
    /// Name of the PVC to create.
    pub name: String,
    /// StorageClass for the new PVC; absent uses the cluster default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    /// Requested size of the new PVC (e.g. `100Gi`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<String>,
    /// Access modes for the new PVC; empty defaults to `[ReadWriteOnce]`. Closed
    /// enum in the schema; a non-canonical value persisted before enforcement
    /// decodes as [`PvcAccessMode::Unknown`] and is rejected per-CR with the
    /// value quoted (webhook + reconciler, via `validate_access_modes`) instead
    /// of poisoning the typed watcher.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access_modes: Vec<PvcAccessMode>,
}

/// kopia restore behavior knobs (M2 flag sweep). Every `Option` field's `None`
/// reproduces kopia's own default — an all-`None`, `enableFileDeletion: false`
/// instance yields the exact same `restore_args` argv produced before these
/// fields existed. The tri-state booleans map to kopia's `--[no-]flag` grammar
/// (`Some(true)` → `--flag`, `Some(false)` → `--no-flag`, `None` → omit).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOptions {
    /// Delete files in the target that are not present in the snapshot (exact mirror); off by default.
    /// Wired to kopia's `--[no-]delete-extra` (previously a silent no-op — see issue #216 gap sweep).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub enable_file_deletion: bool,
    /// Continue past permission errors during restore (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_permission_errors: Option<bool>,
    /// Write files atomically via a temp file + rename (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_files_atomically: Option<bool>,
    /// `--parallel`: restore parallelism (kopia default `8`; `1` disables parallelism).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// `--[no-]write-sparse-files`: attempt to write files sparsely, allocating the
    /// minimum disk space needed (kopia default `false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_sparse_files: Option<bool>,
    /// `--[no-]skip-owners`: skip restoring file owners (kopia default `false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_owners: Option<bool>,
    /// `--[no-]skip-permissions`: skip restoring file permissions (kopia default `false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_permissions: Option<bool>,
    /// `--[no-]skip-times`: skip restoring file modification times (kopia default `false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_times: Option<bool>,
    /// `--[no-]overwrite-files`: overwrite existing files in the target (kopia default `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite_files: Option<bool>,
    /// `--[no-]overwrite-directories`: overwrite existing directories in the target
    /// (kopia default `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite_directories: Option<bool>,
    /// `--[no-]overwrite-symlinks`: overwrite existing symlinks in the target
    /// (kopia default `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite_symlinks: Option<bool>,
    /// `--[no-]ignore-errors`: ignore all restore errors and continue (kopia default `false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_errors: Option<bool>,
    /// `--[no-]skip-existing`: skip files/symlinks that already exist in the target
    /// (kopia default `false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_existing: Option<bool>,
}

/// How the restore reacts to a missing snapshot and how long it waits.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestorePolicy {
    /// What to do when the resolved source matches no snapshot (`Fail`/`Continue`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_missing_snapshot: Option<OnMissingSnapshot>,
    /// How long to wait for the source snapshot to appear before giving up (e.g. `5m`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_timeout: Option<String>,
}

/// What to do when the resolved source matches no snapshot. Defaults to `Fail`
/// (fail-closed) so an explicit restore can never silently no-op; choose
/// `Continue` to provision an empty volume instead (deploy-or-restore).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum OnMissingSnapshot {
    /// Fail-closed; the default for explicit `snapshotRef`/`identity` sources.
    #[default]
    Fail,
    /// Proceed with an empty volume (deploy-or-restore); the default for `fromPolicy`.
    Continue,
}

/// Lifecycle phase of a restore.
///
/// ```
/// use kopiur_api::RestorePhase;
///
/// assert_eq!(serde_json::to_value(RestorePhase::Restoring).unwrap(), "Restoring");
/// // An unrecognized phase from a newer operator decodes instead of erroring.
/// let p: RestorePhase = serde_json::from_value(serde_json::json!("Staging")).unwrap();
/// assert_eq!(p, RestorePhase::Unknown("Staging".into()));
/// assert_eq!(serde_json::to_value(&p).unwrap(), "Staging");
/// assert!(!p.is_terminal());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum RestorePhase {
    /// Admitted but not yet acted on; the default initial phase.
    #[default]
    Pending,
    /// Resolving the source to a concrete snapshot and pinning it to status.
    Resolving,
    /// The mover `Job` is actively writing data into the target.
    Restoring,
    /// The restore finished successfully.
    Completed,
    /// The restore terminally failed; see `conditions` for the reason.
    Failed,
    /// A phase string this build does not recognize (newer operator, or legacy
    /// stored data). Decode-compat only — hidden from the CRD schema, never
    /// produced by this build, never terminal.
    Unknown(String),
}

crate::common::phase_serde!(RestorePhase, "Lifecycle phase of a restore.");

impl RestorePhase {
    /// Whether this phase is **terminal**: the restore reached an end state and
    /// the operator will do no further work on it of its own accord.
    ///
    /// `Pending`, `Resolving`, and `Restoring` are all in-flight — including a
    /// `Pending` restore parked on a structural gate (see
    /// [`crate::gates::STRUCTURAL_GATES`]), which never progresses but is
    /// emphatically not finished. A `Restore` has no deletion phase of its own,
    /// so unlike `SnapshotPhase` there is no wedged-finalizer case to exclude.
    ///
    /// Pure + exhaustive so the single definition lives in one tested place.
    ///
    /// ```
    /// use kopiur_api::RestorePhase;
    ///
    /// assert!(RestorePhase::Completed.is_terminal());
    /// assert!(RestorePhase::Failed.is_terminal());
    /// assert!(!RestorePhase::Pending.is_terminal());
    /// assert!(!RestorePhase::Resolving.is_terminal());
    /// assert!(!RestorePhase::Restoring.is_terminal());
    /// assert!(!RestorePhase::Unknown("Staging".into()).is_terminal());
    /// ```
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Completed | Self::Failed => true,
            Self::Pending | Self::Resolving | Self::Restoring => false,
            // Conservative surface-it policy: a phase this build cannot
            // interpret is never reported as finished, so a newer operator's
            // in-flight restore stays visible to an older CLI/reconciler.
            Self::Unknown(_) => false,
        }
    }

    /// Whether this phase is the **decode sentinel** — a value the running build
    /// cannot interpret, kept verbatim by [`Unknown`](Self::Unknown) instead of
    /// failing the whole typed `list()`/watch (#359, defect 3).
    ///
    /// Same narrow contract as [`SnapshotPhase::is_unknown`](crate::SnapshotPhase::is_unknown):
    /// `true` means only "this string is not a phase this binary knows". A
    /// canonical variant added later is not the sentinel, which is why the
    /// `match` is written out exhaustively rather than left as a `matches!`.
    ///
    /// ```
    /// use kopiur_api::RestorePhase;
    ///
    /// assert!(RestorePhase::Unknown("Staging".into()).is_unknown());
    /// assert!(!RestorePhase::Completed.is_unknown());
    /// assert!(!RestorePhase::Failed.is_unknown());
    /// ```
    pub fn is_unknown(&self) -> bool {
        match self {
            Self::Unknown(_) => true,
            Self::Pending | Self::Resolving | Self::Restoring | Self::Completed | Self::Failed => {
                false
            }
        }
    }
}

impl crate::common::PhaseLabel for RestorePhase {
    const ALL: &'static [Self] = &[
        Self::Pending,
        Self::Resolving,
        Self::Restoring,
        Self::Completed,
        Self::Failed,
    ];
    fn label(&self) -> &str {
        match self {
            Self::Pending => "Pending",
            Self::Resolving => "Resolving",
            Self::Restoring => "Restoring",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Unknown(s) => s,
        }
    }
    fn unknown(raw: String) -> Self {
        Self::Unknown(raw)
    }
}

/// Observed state of a Restore, written by the controller/mover.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestoreStatus {
    /// Current lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<RestorePhase>,
    /// The pinned source kind (`SnapshotRef`/`FromPolicy`/`Identity`); backs the `SOURCE` printer column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    /// `metadata.generation` last reconciled, so stale status is detectable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// The source resolved and pinned at admission; never re-resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedRestore>,
    /// Resolved target details (the PVC written to / populator handshake).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<RestoreTargetStatus>,
    /// Start/end timestamps for the restore run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<RestoreTiming>,
    /// When the `policy.waitTimeout` window OPENED (RFC3339) — the first reconcile on which
    /// the restore could actually proceed (its repository reached `Ready`, and for a
    /// `target.populator` a PVC already claims it), NOT when the Restore was created. The
    /// window does NOT open while the referenced `Repository` object or `fromPolicy`
    /// `SnapshotPolicy` doesn't exist: the restore parks in `Pending`
    /// (`ReferentAvailable=False`, reason `RestoreReferentMissing`) and stays unstamped. It
    /// DOES still open for a `snapshotRef` whose `Snapshot` row doesn't exist yet (so
    /// `onMissingSnapshot` can fire for a ref that never appears) and for a restore whose
    /// mover Job already launched. Stamped once and then honored verbatim, so the window
    /// survives controller restarts and Job pod retries; cleared when a populator re-opens
    /// resolution for a re-created claim, so that claim gets the full window again. Absent
    /// means the window has not opened yet (or no `policy.waitTimeout` is configured, in
    /// which case there is no window to anchor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_started_at: Option<String>,
    /// Bytes/files restored so far, patched periodically by the mover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<RestoreProgress>,
    /// Standard Kubernetes conditions carrying the human-readable status/reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// The last lines of the run's output, written by the mover at the terminal transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_tail: Option<String>,
    /// Structured terminal-failure detail (kopia error class, stderr tail, retry hint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<crate::common::FailureBlock>,
    /// Per-claimant state for a `target.populator` restore, keyed by the claiming
    /// PVC's name (#443).
    ///
    /// A populator `Restore` is claimed by EVERY PVC whose `spec.dataSourceRef`
    /// names it, not just the first, and each claimant gets its own prime PVC, its
    /// own mover `Job` and its own kopia source path — so each needs its own state.
    /// A **map**, not a list, because an RFC-7386 merge patch merges map keys but
    /// REPLACES arrays: N concurrent populate movers each patch only their own key
    /// and can never clobber a sibling (the same reason as
    /// `SnapshotPolicyStatus.verificationStamps`).
    ///
    /// Absent for a direct `target.pvc`/`target.pvcRef` restore, which keeps using
    /// the top-level `resolved`/`target`/`waitStartedAt`/`logTail`/`failure`.
    ///
    /// The schema renders as an object with `additionalProperties`, which PRUNES
    /// unknown keys — so every field any writer (controller or mover) puts under
    /// `claims.<pvc>` must exist on `RestoreClaimStatus`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub claims: std::collections::BTreeMap<String, RestoreClaimStatus>,
}

/// The state of ONE claiming PVC of a `target.populator` restore (#443).
///
/// Written by the controller (every field except `resolved`/`observedAt`/
/// `logTail`/`failure`) and by that claim's mover `Job`, which nests its status
/// patch under `status.claims.<pvc>` and deliberately omits `phase` — the
/// controller owns claim phase exactly as it owns the top-level one.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestoreClaimStatus {
    /// `metadata.uid` of the claiming PVC this record describes. A record whose
    /// uid no longer matches the live claimant is STALE — the PVC was deleted and
    /// re-created — so the claim re-arms and the old claim's prime PVC / Job / PV
    /// are reaped under the recorded uid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    /// Lifecycle phase of this claim; absent means "not observed yet".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<RestoreClaimPhase>,
    /// Machine-readable reason for the current phase (`PopulatingPrimePvc`,
    /// `MoverJobFailed`, `SourcePathAmbiguous`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Human-readable what / why / fix for the current phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The kopia source path this claim restores FROM, derived from the policy's
    /// `sourcePathStrategy` (or pinned from `source.fromPolicy.sourcePath`).
    /// Recorded so a fan-out restore is auditable: each claim shows which member's
    /// data it read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// The source this claim resolved and pinned; never re-resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedRestore>,
    /// The prime PVC provisioned for this claim's populate handshake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pvc_prime: Option<String>,
    /// The mover `Job` populating this claim's prime PVC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// When THIS claim's `policy.waitTimeout` window opened (RFC3339) — the first
    /// pass on which this claim could actually proceed. Per-claim because
    /// claimants appear at different times: a sibling created an hour later gets
    /// its own full window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_started_at: Option<String>,
    /// When this claim's mover last patched (RFC3339). Mover-owned: every
    /// `StatusUpdate` carries it, and the field must exist here or the schema's
    /// `additionalProperties` pruning would silently drop the mover's whole patch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    /// The last lines of this claim's mover output, written once at its terminal
    /// transition. Mover-owned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_tail: Option<String>,
    /// Structured terminal-failure detail for this claim. Mover-owned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<crate::common::FailureBlock>,
}

/// Lifecycle phase of ONE claiming PVC of a populator `Restore` (#443).
///
/// Deliberately a separate closed enum from [`RestorePhase`]: a claim has states
/// the Restore does not (`Rebinding`, `AlreadyBound`), and the Restore-level
/// phase is the AGGREGATE over these. Named `*Phase` on purpose, so
/// `cargo xtask check-phases` polices every branch on it.
///
/// ```
/// use kopiur_api::RestoreClaimPhase;
///
/// assert_eq!(serde_json::to_value(RestoreClaimPhase::Populating).unwrap(), "Populating");
/// // An unrecognized phase from a newer operator decodes instead of erroring.
/// let p: RestoreClaimPhase = serde_json::from_value(serde_json::json!("Staging")).unwrap();
/// assert_eq!(p, RestoreClaimPhase::Unknown("Staging".into()));
/// assert_eq!(serde_json::to_value(&p).unwrap(), "Staging");
/// assert!(!p.is_terminal());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum RestoreClaimPhase {
    /// Observed, but nothing has run for it yet — waiting on a scheduling hint
    /// (`WaitForFirstConsumer`), on the repository, or on the source snapshot.
    #[default]
    Pending,
    /// A mover `Job` is writing this claim's prime PVC.
    Populating,
    /// The prime volume is written and its `PersistentVolume` was rebound to the
    /// claim; waiting for the PV controller to complete the bind.
    Rebinding,
    /// The claim is bound to the volume this restore populated. Terminal.
    Populated,
    /// The claim was ALREADY bound when first observed, so there was nothing to
    /// populate — a CSI volume-populator only fills an UNBOUND claim. A truthful
    /// terminal no-op (#233): no prime PVC, no mover run.
    AlreadyBound,
    /// This claim terminally failed; `reason`/`message` say why. Terminal for the
    /// CLAIM only — siblings keep going, and re-creating this claiming PVC
    /// re-arms it.
    Failed,
    /// A phase string this build does not recognize (newer operator, or legacy
    /// stored data). Decode-compat only — hidden from the CRD schema, never
    /// produced by this build, never terminal.
    Unknown(String),
}

crate::common::phase_serde!(
    RestoreClaimPhase,
    "Lifecycle phase of one claiming PVC of a populator restore."
);

impl RestoreClaimPhase {
    /// Whether this claim reached an end state and the operator will do no
    /// further work on it of its own accord.
    ///
    /// `Failed` IS terminal here even though a populator `Restore` as a whole no
    /// longer short-circuits on `Failed`: the failure is scoped to this claim, and
    /// what re-arms it is deleting and re-creating the claiming PVC (which mints a
    /// new uid, hence a new record) — never a re-drive of the old one.
    ///
    /// Pure + exhaustive, so the single definition lives in one tested place.
    ///
    /// ```
    /// use kopiur_api::RestoreClaimPhase;
    ///
    /// assert!(RestoreClaimPhase::Populated.is_terminal());
    /// assert!(RestoreClaimPhase::AlreadyBound.is_terminal());
    /// assert!(RestoreClaimPhase::Failed.is_terminal());
    /// assert!(!RestoreClaimPhase::Pending.is_terminal());
    /// assert!(!RestoreClaimPhase::Populating.is_terminal());
    /// assert!(!RestoreClaimPhase::Rebinding.is_terminal());
    /// assert!(!RestoreClaimPhase::Unknown("Staging".into()).is_terminal());
    /// ```
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Populated | Self::AlreadyBound | Self::Failed => true,
            Self::Pending | Self::Populating | Self::Rebinding => false,
            // Conservative surface-it policy, same as `RestorePhase::is_terminal`:
            // a phase this build cannot interpret is never reported as finished.
            Self::Unknown(_) => false,
        }
    }

    /// Whether this phase is the **decode sentinel** — a value the running build
    /// cannot interpret, kept verbatim by [`Unknown`](Self::Unknown) instead of
    /// failing the whole typed `list()`/watch for the Kind.
    ///
    /// ```
    /// use kopiur_api::RestoreClaimPhase;
    ///
    /// assert!(RestoreClaimPhase::Unknown("Staging".into()).is_unknown());
    /// assert!(!RestoreClaimPhase::Populated.is_unknown());
    /// ```
    pub fn is_unknown(&self) -> bool {
        match self {
            Self::Unknown(_) => true,
            Self::Pending
            | Self::Populating
            | Self::Rebinding
            | Self::Populated
            | Self::AlreadyBound
            | Self::Failed => false,
        }
    }
}

impl crate::common::PhaseLabel for RestoreClaimPhase {
    const ALL: &'static [Self] = &[
        Self::Pending,
        Self::Populating,
        Self::Rebinding,
        Self::Populated,
        Self::AlreadyBound,
        Self::Failed,
    ];
    fn label(&self) -> &str {
        match self {
            Self::Pending => "Pending",
            Self::Populating => "Populating",
            Self::Rebinding => "Rebinding",
            Self::Populated => "Populated",
            Self::AlreadyBound => "AlreadyBound",
            Self::Failed => "Failed",
            Self::Unknown(s) => s,
        }
    }
    fn unknown(raw: String) -> Self {
        Self::Unknown(raw)
    }
}

/// Which outcome the source resolution pinned, once and never re-resolved.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum ResolutionOutcome {
    /// The source resolved to a concrete kopia snapshot (see `kopiaSnapshotID`).
    Snapshot,
    /// The source matched no snapshot; `Continue` chose an empty (deploy-or-restore) volume.
    NoSnapshot,
}

/// The source resolved and pinned at admission, so a restore never silently retargets.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRestore {
    /// Which outcome the resolution pinned (`Snapshot`/`NoSnapshot`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<ResolutionOutcome>,
    /// The exact kopia snapshot manifest id the source resolved to; pinned once.
    #[serde(
        default,
        rename = "kopiaSnapshotID",
        skip_serializing_if = "Option::is_none"
    )]
    pub kopia_snapshot_id: Option<String>,
    /// The concrete `Snapshot` CR the source resolved to, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_ref: Option<ObjectRef>,
    /// The repository the snapshot lives in, resolved from the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryRef>,
    /// Timestamp at which the source was pinned (RFC3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<String>,
    /// The resolved kopia identity (`username@hostname:path`) of the snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ResolvedIdentity>,
}

/// Resolved restore target details written to status.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestoreTargetStatus {
    /// Populator handshake (passive / pvc-create modes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pvc_prime: Option<String>,
    /// The PVC actually written to (created or pre-existing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pvc_ref: Option<ObjectRef>,
}

/// Start/end timestamps of a restore run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestoreTiming {
    /// When the mover began restoring (RFC3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    /// When the restore reached a terminal phase (RFC3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
}

/// Live progress counters patched by the mover during a restore.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestoreProgress {
    /// Total bytes restored so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_restored: Option<i64>,
    /// Total files restored so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_restored: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::PhaseLabel;
    use crate::testutil::from_yaml;
    use kube::core::CustomResourceExt;

    #[test]
    fn restore_phase_all_covers_every_variant_uniquely() {
        // Mirrors the `SnapshotPhase` tripwire: every variant is in ALL with a
        // unique, non-empty label. A variant added without updating ALL fails
        // here (and `label`'s exhaustive match won't compile at all).
        let labels: Vec<&str> = RestorePhase::ALL.iter().map(|p| p.label()).collect();
        assert_eq!(RestorePhase::ALL.len(), 5);
        assert!(labels.iter().all(|l| !l.is_empty()));
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "phase labels must be unique");
        assert!(RestorePhase::ALL.contains(&RestorePhase::default()));
    }

    #[test]
    fn restore_terminal_set_is_pinned() {
        // Driven off ALL so a new variant must be classified deliberately.
        let terminal: Vec<&str> = RestorePhase::ALL
            .iter()
            .filter(|p| p.is_terminal())
            .map(|p| p.label())
            .collect();
        assert_eq!(terminal, ["Completed", "Failed"]);
        let in_flight: Vec<&str> = RestorePhase::ALL
            .iter()
            .filter(|p| !p.is_terminal())
            .map(|p| p.label())
            .collect();
        assert_eq!(in_flight, ["Pending", "Resolving", "Restoring"]);
    }

    #[test]
    fn restore_crd_metadata_is_correct() {
        let crd = Restore::crd();
        assert_eq!(crd.spec.group, "kopiur.home-operations.com");
        assert_eq!(crd.spec.names.kind, "Restore");
        assert_eq!(crd.spec.scope, "Namespaced");
        assert_eq!(crd.spec.versions[0].name, "v1alpha1");
    }

    #[test]
    fn restore_crd_carries_target_xor_x_kubernetes_validation() {
        // §15: the generated CRD spec schema must carry the operator-authored
        // x-kubernetes-validations rule (exactly-one-of target.*) at the spec level,
        // surviving kube's structural-schema rewriter.
        let crd = Restore::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let spec_schema =
            &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"];
        let rules = spec_schema["x-kubernetes-validations"]
            .as_array()
            .expect("spec.x-kubernetes-validations present");
        assert!(
            rules.iter().any(|r| r["rule"]
                .as_str()
                .is_some_and(|s| s.contains("target.populator"))),
            "expected the target XOR rule; got {rules:?}"
        );
    }

    #[test]
    fn from_policy_offset_carries_static_openapi_default_in_crd() {
        // ADR-0005 §1: source.fromPolicy.offset must carry a real schema `default: 0`.
        let crd = Restore::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let default = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["source"]["properties"]["fromPolicy"]["properties"]["offset"]["default"];
        assert_eq!(
            default, 0,
            "fromPolicy.offset must emit `default: 0` in the CRD schema; got {default:?}"
        );
    }

    #[test]
    fn from_policy_offset_defaults_to_zero_when_absent() {
        let spec: RestoreSpec =
            from_yaml("source: { fromPolicy: { name: pg } }\ntarget: { populator: {} }\n");
        match &spec.source {
            RestoreSource::FromPolicy(c) => assert_eq!(c.offset, 0),
            other => panic!("expected FromPolicy, got {}", other.kind_str()),
        }
    }

    #[test]
    fn restore_backup_ref_roundtrip_matches_adr_shape() {
        // Mirrors ADR-0001 §3.6 / §5.3.
        let yaml = r#"
source:
  snapshotRef: { name: postgres-data-20260524-021300, namespace: billing }
target:
  pvc:
    name: postgres-data-restored
    storageClassName: fast-ssd
    capacity: 100Gi
    accessModes: [ReadWriteOnce]
options:
  enableFileDeletion: false
  ignorePermissionErrors: true
  writeFilesAtomically: true
policy:
  onMissingSnapshot: Fail
  waitTimeout: 5m
"#;
        let spec: RestoreSpec = from_yaml(yaml);
        assert_eq!(spec.source.kind_str(), "SnapshotRef");
        match &spec.source {
            RestoreSource::SnapshotRef(r) => {
                assert_eq!(r.name, "postgres-data-20260524-021300");
                assert_eq!(r.namespace.as_deref(), Some("billing"));
            }
            other => panic!("expected SnapshotRef, got {}", other.kind_str()),
        }
        let target = &spec.target;
        assert_eq!(target.kind_str(), "Pvc");
        match target {
            RestoreTarget::Pvc(t) => {
                assert_eq!(t.name, "postgres-data-restored");
                assert_eq!(t.access_modes, vec![PvcAccessMode::ReadWriteOnce]);
            }
            other => panic!("expected Pvc, got {}", other.kind_str()),
        }
        assert_eq!(
            spec.policy.as_ref().unwrap().on_missing_snapshot,
            Some(OnMissingSnapshot::Fail)
        );

        let json = serde_json::to_value(&spec).expect("serialize");
        let reparsed: RestoreSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn restore_pvc_access_modes_render_a_closed_enum_in_the_crd_schema() {
        // The Vec<String> → Vec<PvcAccessMode> migration must surface in the CRD:
        // items are a closed string enum (apiserver rejects typos on new writes),
        // and the legacy-decode `Unknown` variant never appears.
        let crd = Restore::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let items = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["target"]["properties"]["pvc"]["properties"]["accessModes"]["items"];
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
    fn restore_legacy_access_mode_decodes_to_unknown_not_an_error() {
        // A pre-enforcement stored Restore with a bogus mode must still
        // deserialize (a serde error would poison the typed watch stream for the
        // whole Kind) — the value lands in `Unknown`, verbatim, and the shared
        // validator rejects it per-CR with the value quoted.
        let spec: RestoreSpec = from_yaml(
            "source: { snapshotRef: { name: b } }\n\
             target: { pvc: { name: restored, capacity: 10Gi, accessModes: [ReadWriteOnze] } }\n",
        );
        match &spec.target {
            RestoreTarget::Pvc(t) => assert_eq!(
                t.access_modes,
                vec![PvcAccessMode::Unknown("ReadWriteOnze".into())]
            ),
            other => panic!("expected Pvc, got {}", other.kind_str()),
        }
        // And it re-serializes unchanged (read-modify-write never mutates it).
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(
            json["target"]["pvc"]["accessModes"],
            serde_json::json!(["ReadWriteOnze"])
        );
    }

    #[test]
    fn restore_options_full_flag_sweep_roundtrip() {
        // M2 flag sweep: every new `options` knob round-trips through the cluster's
        // YAML → serde_json::Value → typed path.
        let yaml = r#"
source: { snapshotRef: { name: b } }
target: { pvcRef: { name: d } }
options:
  enableFileDeletion: true
  ignorePermissionErrors: false
  writeFilesAtomically: true
  parallel: 4
  writeSparseFiles: true
  skipOwners: true
  skipPermissions: false
  skipTimes: true
  overwriteFiles: false
  overwriteDirectories: false
  overwriteSymlinks: true
  ignoreErrors: false
  skipExisting: true
"#;
        let spec: RestoreSpec = from_yaml(yaml);
        let o = spec.options.as_ref().expect("options set");
        assert!(o.enable_file_deletion);
        assert_eq!(o.ignore_permission_errors, Some(false));
        assert_eq!(o.write_files_atomically, Some(true));
        assert_eq!(o.parallel, Some(4));
        assert_eq!(o.write_sparse_files, Some(true));
        assert_eq!(o.skip_owners, Some(true));
        assert_eq!(o.skip_permissions, Some(false));
        assert_eq!(o.skip_times, Some(true));
        assert_eq!(o.overwrite_files, Some(false));
        assert_eq!(o.overwrite_directories, Some(false));
        assert_eq!(o.overwrite_symlinks, Some(true));
        assert_eq!(o.ignore_errors, Some(false));
        assert_eq!(o.skip_existing, Some(true));

        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["options"]["parallel"], 4);
        assert_eq!(json["options"]["skipExisting"], true);
        let reparsed: RestoreSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn restore_options_omits_unset_leaf_fields() {
        // A minimal `options` block that only sets `enableFileDeletion` must not
        // serialize the other (unset) knobs.
        let yaml = r#"
source: { snapshotRef: { name: b } }
target: { pvcRef: { name: d } }
options:
  enableFileDeletion: true
"#;
        let spec: RestoreSpec = from_yaml(yaml);
        let json = serde_json::to_value(&spec).unwrap();
        let opts_json = &json["options"];
        assert_eq!(opts_json["enableFileDeletion"], true);
        for key in [
            "ignorePermissionErrors",
            "writeFilesAtomically",
            "parallel",
            "writeSparseFiles",
            "skipOwners",
            "skipPermissions",
            "skipTimes",
            "overwriteFiles",
            "overwriteDirectories",
            "overwriteSymlinks",
            "ignoreErrors",
            "skipExisting",
        ] {
            assert!(opts_json.get(key).is_none(), "{key} should be absent");
        }
    }

    #[test]
    fn restore_passive_populator_mode_uses_explicit_populator_target() {
        // ADR-0005 §9: passive populator mode is now an EXPLICIT `target.populator: {}`
        // (the empty-`target` form is removed). Mirrors ADR-0001 §5.5
        // deploy-or-restore: fromPolicy + Continue + populator.
        let yaml = r#"
source: { fromPolicy: { name: postgres-data, offset: 0 } }
target: { populator: {} }
policy: { onMissingSnapshot: Continue }
"#;
        let spec: RestoreSpec = from_yaml(yaml);
        assert_eq!(spec.source.kind_str(), "FromPolicy");
        assert_eq!(spec.target.kind_str(), "Populator");
        assert!(matches!(spec.target, RestoreTarget::Populator(_)));
        match &spec.source {
            RestoreSource::FromPolicy(c) => {
                assert_eq!(c.name, "postgres-data");
                assert_eq!(c.offset, 0);
            }
            other => panic!("expected FromPolicy, got {}", other.kind_str()),
        }

        // Externally tagged: `{ populator: {} }`.
        let json = serde_json::to_value(&spec).unwrap();
        assert!(json["target"]["populator"].is_object());
        let reparsed: RestoreSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn restore_without_target_fails_to_deserialize() {
        // ADR-0005 §9 breaking change: a Restore with no `target` is invalid.
        let value: serde_json::Value =
            serde_yaml::from_str("source: { snapshotRef: { name: b } }\n").unwrap();
        assert!(
            serde_json::from_value::<RestoreSpec>(value).is_err(),
            "an absent target must be rejected (no empty-target form, ADR-0005 §9)"
        );
    }

    #[test]
    fn restore_populator_rejects_inherit_security_context() {
        // ADR-0005 §9: inheritSecurityContextFrom is meaningless with a populator
        // target (no workload pod exists at provision time) — the validator rejects it.
        use crate::common::{InheritSecurityContextFrom, MoverSpec, PodSelector};
        use crate::validate::validate_restore;
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
        let spec = RestoreSpec {
            repository: None,
            source: RestoreSource::FromPolicy(FromPolicy {
                name: "pg".into(),
                namespace: None,
                as_of: None,
                offset: 0,
                source_path: None,
            }),
            target: RestoreTarget::Populator(PopulatorTarget {}),
            options: None,
            policy: None,
            credential_projection: None,
            mover: Some(MoverSpec {
                inherit_security_context_from: Some(InheritSecurityContextFrom::WorkloadSelector(
                    PodSelector {
                        pod_selector: LabelSelector::default(),
                        container: None,
                    },
                )),
                ..Default::default()
            }),
            failure_policy: None,
        };
        assert!(matches!(
            validate_restore(&spec),
            Err(crate::error::ValidationError::InvalidFieldValue { .. })
        ));

        // The same populator target WITHOUT inherit is fine.
        let ok = RestoreSpec {
            mover: None,
            ..spec
        };
        assert!(validate_restore(&ok).is_ok());
    }

    #[test]
    fn restore_identity_source_requires_repository_in_practice() {
        // The `identity` source variant; spec.repository is webhook-required (not type-required).
        let yaml = r#"
repository: { kind: Repository, name: nas-primary, namespace: backups }
source:
  identity:
    username: postgres-data
    hostname: billing
    sourcePath: /data
    snapshotID: k1f1ec0a8
target:
  pvcRef: { name: postgres-data-restored }
"#;
        let spec: RestoreSpec = from_yaml(yaml);
        assert_eq!(spec.source.kind_str(), "Identity");
        assert!(spec.repository.is_some());
        match &spec.source {
            RestoreSource::Identity(i) => {
                assert_eq!(i.username, "postgres-data");
                assert_eq!(i.snapshot_id.as_deref(), Some("k1f1ec0a8"));
            }
            other => panic!("expected Identity, got {}", other.kind_str()),
        }
        assert_eq!(spec.target.kind_str(), "PvcRef");

        let json = serde_json::to_value(&spec).unwrap();
        let reparsed: RestoreSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn restore_mover_and_failure_policy_roundtrip() {
        // Restore carries the same mover surface a backup gets (resources +
        // securityContext for UID/GID match + cache) plus a failurePolicy.
        let yaml = r#"
source: { snapshotRef: { name: app-data-backup } }
target: { pvcRef: { name: app-data-restored } }
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
failurePolicy:
  backoffLimit: 4
  activeDeadlineSeconds: 3600
"#;
        let spec: RestoreSpec = from_yaml(yaml);
        let mover = spec.mover.as_ref().expect("mover");
        assert!(mover.resources.is_some());
        assert_eq!(
            mover.cache.as_ref().and_then(|c| c.capacity.as_deref()),
            Some("16Gi")
        );
        assert_eq!(
            mover.security_context.as_ref().and_then(|s| s.run_as_user),
            Some(1000)
        );
        // fsGroup is carried on the pod-level securityContext (makes a fresh restore
        // volume group-writable for an unprivileged mover).
        assert_eq!(
            mover.pod_security_context.as_ref().and_then(|p| p.fs_group),
            Some(1000)
        );
        // A hardened non-root container + fsGroup is NOT privileged: the gate lets it run.
        assert!(!mover.requires_privilege());
        let fp = spec.failure_policy.as_ref().expect("failurePolicy");
        assert_eq!(fp.backoff_limit, Some(4));
        assert_eq!(fp.active_deadline_seconds, Some(3600));

        let json = serde_json::to_value(&spec).expect("serialize");
        let reparsed: RestoreSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn restore_mover_root_context_is_privileged() {
        // `runAsUser: 0` on a restore mover trips the same privileged-mover gate as a
        // backup — the controller refuses it unless the namespace opts in.
        let yaml = r#"
source: { snapshotRef: { name: app-data-backup } }
target: { pvcRef: { name: app-data-restored } }
mover:
  securityContext:
    runAsUser: 0
    runAsNonRoot: false
"#;
        let spec: RestoreSpec = from_yaml(yaml);
        assert!(spec.mover.as_ref().unwrap().requires_privilege());
    }

    #[test]
    fn restore_crd_schema_carries_the_snapshot_inherit_variant() {
        // `Restore::crd()`/`SnapshotPolicy::crd()` smoke for the new externally-tagged
        // variant: schema generation must not panic, and the Restore schema must
        // surface `inheritSecurityContextFrom.snapshot` as an object property (the
        // SnapshotPolicy schema carries it too — the restriction is webhook-level,
        // not structural).
        let crd = Restore::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let inherit = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["mover"]["properties"]["inheritSecurityContextFrom"]["properties"];
        assert!(
            inherit["snapshot"].is_object(),
            "inheritSecurityContextFrom must carry the `snapshot` variant; got {inherit}"
        );
        assert_eq!(inherit["snapshot"]["type"], "object");
        // The other variants are untouched.
        assert!(inherit["workloadSelector"].is_object());
        assert!(inherit["pvcConsumer"].is_object());
        let _ = crate::SnapshotPolicy::crd();
    }

    #[test]
    fn restore_snapshot_inherit_mover_roundtrip() {
        // The restore-only recorded-identity inherit mode, parsed the cluster's way.
        use crate::common::{InheritSecurityContextFrom, SnapshotInherit};
        let yaml = r#"
source: { snapshotRef: { name: app-data-backup } }
target: { pvcRef: { name: app-data-restored } }
mover:
  inheritSecurityContextFrom:
    snapshot: {}
"#;
        let spec: RestoreSpec = from_yaml(yaml);
        assert!(matches!(
            spec.mover
                .as_ref()
                .and_then(|m| m.inherit_security_context_from.as_ref()),
            Some(InheritSecurityContextFrom::Snapshot(SnapshotInherit {})),
        ));
        let json = serde_json::to_value(&spec).expect("serialize");
        assert!(json["mover"]["inheritSecurityContextFrom"]["snapshot"].is_object());
        let reparsed: RestoreSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn restore_source_unknown_variant_is_rejected() {
        let value: serde_json::Value = serde_yaml::from_str("snapshotUrl:\n  url: x\n").unwrap();
        assert!(serde_json::from_value::<RestoreSource>(value).is_err());
    }

    #[test]
    fn on_missing_snapshot_and_phase_serialize_to_expected_strings() {
        assert_eq!(
            serde_json::to_value(OnMissingSnapshot::Fail).unwrap(),
            "Fail"
        );
        assert_eq!(
            serde_json::to_value(OnMissingSnapshot::Continue).unwrap(),
            "Continue"
        );
        assert_eq!(
            serde_json::to_value(RestorePhase::Restoring).unwrap(),
            "Restoring"
        );
        assert_eq!(
            serde_json::to_value(RestorePhase::Completed).unwrap(),
            "Completed"
        );
    }

    // --- #443: per-claim status ------------------------------------------

    #[test]
    fn restore_claim_phase_all_covers_every_variant_uniquely() {
        // Same tripwire as `RestorePhase`: every canonical variant is in ALL with
        // a unique, non-empty label. A variant added without updating ALL fails
        // here (and `label`'s exhaustive match won't compile at all).
        let labels: Vec<&str> = RestoreClaimPhase::ALL.iter().map(|p| p.label()).collect();
        assert_eq!(RestoreClaimPhase::ALL.len(), 6);
        assert!(labels.iter().all(|l| !l.is_empty()));
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "phase labels must be unique");
        assert!(RestoreClaimPhase::ALL.contains(&RestoreClaimPhase::default()));
    }

    #[test]
    fn restore_claim_terminal_set_is_pinned() {
        // Driven off ALL so a new variant must be classified deliberately.
        let terminal: Vec<&str> = RestoreClaimPhase::ALL
            .iter()
            .filter(|p| p.is_terminal())
            .map(|p| p.label())
            .collect();
        assert_eq!(terminal, ["Populated", "AlreadyBound", "Failed"]);
        let in_flight: Vec<&str> = RestoreClaimPhase::ALL
            .iter()
            .filter(|p| !p.is_terminal())
            .map(|p| p.label())
            .collect();
        assert_eq!(in_flight, ["Pending", "Populating", "Rebinding"]);
        // The decode sentinel is never in ALL and never terminal.
        assert!(!RestoreClaimPhase::ALL.iter().any(|p| p.is_unknown()));
    }

    #[test]
    fn restore_claim_phase_round_trips_and_tolerates_an_unknown_value() {
        for p in RestoreClaimPhase::ALL {
            let json = serde_json::to_value(p).expect("serialize");
            assert_eq!(json, p.label());
            let back: RestoreClaimPhase = serde_json::from_value(json).expect("decode");
            assert_eq!(&back, p);
        }
        // A phase written by a newer operator must not poison the typed watch.
        let unknown: RestoreClaimPhase =
            serde_json::from_value(serde_json::json!("Quiescing")).expect("decodes");
        assert_eq!(unknown, RestoreClaimPhase::Unknown("Quiescing".into()));
        assert!(unknown.is_unknown());
        assert!(!unknown.is_terminal());
        // …and is echoed back verbatim, so a read-modify-write never mutates it.
        assert_eq!(serde_json::to_value(&unknown).unwrap(), "Quiescing");
    }

    #[test]
    fn restore_claim_phase_schema_publishes_only_canonical_values() {
        // `Unknown` is a decode-compat artifact, never an admissible write.
        let crd = Restore::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let phase = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["status"]
            ["properties"]["claims"]["additionalProperties"]["properties"]["phase"];
        let values: Vec<String> = phase["enum"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            values,
            RestoreClaimPhase::canonical(),
            "claim phase schema must be exactly the canonical set; got {phase}"
        );
    }

    #[test]
    fn restore_status_claims_render_as_an_additional_properties_map() {
        // The map schema is what makes N concurrent movers safe (merge-patch
        // merges map keys, replaces arrays). `additionalProperties` PRUNES unknown
        // keys, so the per-claim object must declare every field any writer sets —
        // including the mover-owned `observedAt`, which every `StatusUpdate`
        // carries and whose absence would drop the mover's whole patch.
        let crd = Restore::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let claims = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["status"]
            ["properties"]["claims"];
        assert_eq!(claims["type"], "object", "got {claims}");
        let props = claims["additionalProperties"]["properties"]
            .as_object()
            .expect("per-claim properties");
        for key in [
            "uid",
            "phase",
            "reason",
            "message",
            "sourcePath",
            "resolved",
            "pvcPrime",
            "job",
            "waitStartedAt",
            "observedAt",
            "logTail",
            "failure",
        ] {
            assert!(props.contains_key(key), "missing `{key}` in {claims}");
        }
    }

    #[test]
    fn restore_status_claims_round_trip_through_the_apiserver_shape() {
        let status: RestoreStatus = from_yaml(
            r#"
phase: Restoring
claims:
  data-0:
    uid: 11111111-2222-3333-4444-555555555555
    phase: Populated
    reason: RestoreSucceeded
    sourcePath: /pvc/data-0
    pvcPrime: prime-1111
    job: r-populate-deadbeef
    observedAt: "2026-09-01T00:00:00Z"
    resolved:
      resolution: Snapshot
      kopiaSnapshotID: k1
  data-1:
    uid: 66666666-2222-3333-4444-555555555555
    phase: Failed
    reason: MoverJobFailed
"#,
        );
        assert_eq!(status.claims.len(), 2);
        let a = &status.claims["data-0"];
        assert_eq!(a.phase, Some(RestoreClaimPhase::Populated));
        assert_eq!(a.source_path.as_deref(), Some("/pvc/data-0"));
        assert_eq!(a.job.as_deref(), Some("r-populate-deadbeef"));
        assert_eq!(
            a.resolved
                .as_ref()
                .and_then(|r| r.kopia_snapshot_id.as_deref()),
            Some("k1")
        );
        assert_eq!(
            status.claims["data-1"].phase,
            Some(RestoreClaimPhase::Failed)
        );

        // Structural round-trip: what we serialize decodes back identically.
        let json = serde_json::to_value(&status).expect("serialize");
        let reparsed: RestoreStatus = serde_json::from_value(json).expect("reparse");
        assert_eq!(status, reparsed);

        // An empty map is omitted entirely, so a DIRECT restore's status is
        // byte-identical to what it was before #443.
        let direct = RestoreStatus::default();
        let json = serde_json::to_value(&direct).expect("serialize");
        assert!(
            json.get("claims").is_none(),
            "an empty claims map must not be written: {json}"
        );
    }

    #[test]
    fn from_policy_round_trips_with_and_without_source_path() {
        let with: RestoreSpec = from_yaml(
            "source: { fromPolicy: { name: pg, sourcePath: /pvc/pgdata } }\n\
             target: { populator: {} }\n",
        );
        match &with.source {
            RestoreSource::FromPolicy(c) => {
                assert_eq!(c.source_path.as_deref(), Some("/pvc/pgdata"));
                assert_eq!(c.offset, 0);
            }
            other => panic!("expected FromPolicy, got {}", other.kind_str()),
        }
        let json = serde_json::to_value(&with).expect("serialize");
        assert_eq!(json["source"]["fromPolicy"]["sourcePath"], "/pvc/pgdata");
        let reparsed: RestoreSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(with, reparsed);

        // Absent stays absent on the wire — no new key on an existing object.
        let without: RestoreSpec =
            from_yaml("source: { fromPolicy: { name: pg } }\ntarget: { populator: {} }\n");
        match &without.source {
            RestoreSource::FromPolicy(c) => assert_eq!(c.source_path, None),
            other => panic!("expected FromPolicy, got {}", other.kind_str()),
        }
        let json = serde_json::to_value(&without).expect("serialize");
        assert!(
            json["source"]["fromPolicy"].get("sourcePath").is_none(),
            "{json}"
        );
    }

    #[test]
    fn from_policy_source_path_is_a_bounded_string_in_the_crd_schema() {
        let crd = Restore::crd();
        let json = serde_json::to_value(&crd).expect("serialize CRD");
        let field = &json["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["source"]["properties"]["fromPolicy"]["properties"]["sourcePath"];
        assert_eq!(field["type"], "string", "got {field}");
        assert_eq!(field["maxLength"], 4096, "got {field}");
    }
}
