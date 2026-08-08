//! The `Snapshot` CRD — a single kopia snapshot as a Kubernetes object.
//! ADR-0001 §3.4, ADR-0003 §4.5.
//!
//! Three origins (canonical value lives in `status.origin`):
//! - `scheduled` — created by a `SnapshotSchedule`; spec carries `policyRef`.
//! - `manual`    — created by `kubectl create` / external automation; spec carries `policyRef`.
//! - `discovered`— materialized by the catalog scan; spec is empty/absent.

use crate::common::{
    CredentialProjection, DeletionPolicy, FailurePolicy, PolicyRef, RepositoryRef,
    ResolvedIdentity, ScheduleDeletePolicy,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single kopia snapshot represented as a Kubernetes object.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "kopiur.home-operations.com",
    version = "v1alpha1",
    kind = "Snapshot",
    namespaced,
    status = "SnapshotStatus",
    shortname = "kopiasnap",
    category = "kopiur",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Origin","type":"string","jsonPath":".status.origin"}"#,
    printcolumn = r#"{"name":"Snapshot","type":"string","jsonPath":".status.snapshot.kopiaSnapshotID"}"#,
    // The PVC this run covers. Blank for a single-source policy (the recipe's
    // one source IS the answer); populated for every child of a `pvcSelector`
    // expansion, where a bare `kubectl get snapshots` would otherwise show N
    // rows with the same policy and no way to tell them apart.
    printcolumn = r#"{"name":"Source","type":"string","jsonPath":".spec.source.target.pvc.name"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSpec {
    /// The `SnapshotPolicy` recipe to run; absent for `discovered` backups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_ref: Option<PolicyRef>,
    /// The ONE concrete source this `Snapshot` covers, when `policyRef` names a
    /// recipe whose `sources[]` expands to many — i.e. a
    /// [`pvcSelector`](crate::snapshot_policy::PvcSelector).
    ///
    /// Stamped by whoever minted the CR: a `SnapshotSchedule` fire, or
    /// `kubectl kopiur snapshot now`. Absent for the ordinary single-source
    /// case, where the policy's own `sources[0]` is the target.
    ///
    /// Absent against a *selector* policy is refused rather than guessed. The
    /// operator must never pick a PVC on the user's behalf: silently backing up
    /// one arbitrary volume out of N looks exactly like success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SnapshotSourceRef>,
    /// Free-form tags attached to the kopia snapshot manifest itself
    /// (`snapshot create --tags`), e.g. `reason: pre-upgrade` — durable in the
    /// repository, independent of this CR. Keys must be non-empty, colon-free
    /// (kopia splits on the first colon), and must not start with the reserved
    /// `kopiur` prefix; at most 10 tags, keys ≤ 63 bytes, values ≤ 256 bytes
    /// (webhook-enforced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<BTreeMap<String, String>>,
    /// Mover Job retry and deadline limits for this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_policy: Option<FailurePolicy>,
    /// What happens to the kopia snapshot when this CR is deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_policy: Option<DeletionPolicy>,
    /// What the schedule-deletion cascade does to this Snapshot: consulted by the
    /// finalizer ONLY when the deletion is external (not an operator prune) and the
    /// owning `SnapshotSchedule` is gone or replaced (ownerRef UID mismatch).
    /// `Retain` downgrades an effective `Delete` so the kopia snapshot survives;
    /// `Delete` lets the Snapshot's own `deletionPolicy` cascade. Stamped at
    /// creation from the schedule's `spec.deletion.onScheduleDelete`; absent
    /// (pre-upgrade Snapshots, manual/discovered Snapshots) resolves to `Retain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_schedule_delete: Option<ScheduleDeletePolicy>,
    /// Exempt this snapshot from GFS retention.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pin: bool,
    /// Free-form text recorded on the kopia snapshot manifest
    /// (`snapshot create --description`). Per-invocation by nature —
    /// scheduled/discovered `Snapshot`s never set this (no templated
    /// descriptions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1024))]
    pub description: Option<String>,
}

/// Which source of the referenced `SnapshotPolicy` this `Snapshot` covers, and
/// what that source resolved to at expansion time.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSourceRef {
    /// Zero-based index into `policyRef`'s `spec.sources` this child expanded
    /// from.
    ///
    /// Pins WHICH source's knobs (`readOnly`, `sourcePathOverride`,
    /// `sourcePathStrategy`, `acknowledgeLiveMutation`) govern this run, so a
    /// policy carrying several sources stays unambiguous. An index that is out
    /// of range at reconcile time — the policy shrank mid-run — is a named
    /// terminal failure, never a silent fallback to `sources[0]`.
    pub source_index: u32,
    /// What the expansion resolved to.
    pub target: SnapshotSourceTarget,
    /// The consistency group this child belongs to, present only when the
    /// policy asked for one (`groupBy: VolumeGroupSnapshot`) AND the expansion
    /// produced more than one member in this namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<SnapshotSourceGroup>,
}

/// The resolved target of one expanded source.
///
/// Externally tagged (`target: { pvc: {...} }`) per the repo's
/// discriminated-union rule: internally-tagged enums break Kubernetes
/// structural-schema generation. A future expandable source kind cannot compile
/// until every handler accounts for it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotSourceTarget {
    /// One `PersistentVolumeClaim`, fully qualified.
    Pvc(PvcTargetRef),
}

/// A fully-qualified `PersistentVolumeClaim` reference.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PvcTargetRef {
    /// Namespace of the matched `PersistentVolumeClaim`.
    ///
    /// Explicit rather than inferred from the `Snapshot`'s own namespace: a
    /// `pvcSelector` under a `ClusterRepository` may match across namespaces.
    pub namespace: String,
    /// Name of the matched `PersistentVolumeClaim`.
    pub name: String,
}

/// The shared CSI `VolumeGroupSnapshot` every member of one expansion stages
/// from.
///
/// Pinned to the SPEC, not derived per reconcile, for the same reason
/// `status.staged.stagingTimeoutSeconds` is pinned: the group is an
/// *invocation*-time decision, and a policy edited or deleted mid-run must
/// never move the object a live member is waiting on — or reaping.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSourceGroup {
    /// Namespace the `VolumeGroupSnapshot` lives in.
    ///
    /// A `VolumeGroupSnapshot` is namespaced and its `source.selector` is
    /// namespace-local, so a selector spanning namespaces yields ONE GROUP PER
    /// NAMESPACE, not one group. The consistency guarantee is per-namespace and
    /// this field is where that shows.
    pub namespace: String,
    /// Name of the shared `VolumeGroupSnapshot`.
    pub volume_group_snapshot_name: String,
}

/// How a `Snapshot` came to exist. Canonical value mirrored from the
/// `kopiur.home-operations.com/origin` label. Origin drives the deletion-policy
/// default: `discovered` backups are forced to `Retain` because the operator did
/// not create those snapshots.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Origin {
    /// Created by a `SnapshotSchedule`; spec carries `policyRef`.
    #[default]
    Scheduled,
    /// Created by `kubectl create` / external automation; spec carries `policyRef`.
    Manual,
    /// Materialized by the catalog scan for a snapshot kopiur didn't produce.
    Discovered,
    /// A `discovered` snapshot whose resolved identity matched a live
    /// `SnapshotPolicy` and was automatically (or explicitly) re-attached to
    /// it: it now carries that policy's config label and is retention-governed
    /// like any produced row, even though the operator did not create the
    /// underlying kopia snapshot.
    Adopted,
}

/// Lifecycle phase of a `Snapshot`.
///
/// ```
/// use kopiur_api::SnapshotPhase;
/// use kopiur_api::common::PhaseLabel;
///
/// // Canonical values round-trip as bare strings.
/// assert_eq!(serde_json::to_value(SnapshotPhase::Succeeded).unwrap(), "Succeeded");
/// let p: SnapshotPhase = serde_json::from_value(serde_json::json!("Running")).unwrap();
/// assert_eq!(p, SnapshotPhase::Running);
///
/// // A phase written by a NEWER operator decodes into `Unknown` (never a
/// // watcher-poisoning serde error) and re-serializes verbatim.
/// let p: SnapshotPhase = serde_json::from_value(serde_json::json!("Quiescing")).unwrap();
/// assert_eq!(p, SnapshotPhase::Unknown("Quiescing".into()));
/// assert_eq!(serde_json::to_value(&p).unwrap(), "Quiescing");
/// assert_eq!(p.label(), "Quiescing");
/// // Never terminal: an unrecognized phase is held and surfaced, not finished.
/// assert!(!p.is_terminal());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum SnapshotPhase {
    /// Admitted, not yet started (also the default).
    #[default]
    Pending,
    /// Mover Job is in flight.
    Running,
    /// Snapshot created successfully.
    Succeeded,
    /// Mover Job exhausted its retries.
    Failed,
    /// CR is being deleted; finalizer is reclaiming the snapshot.
    Deleting,
    /// Catalog-materialized backup kopiur didn't produce.
    Discovered,
    /// The backup ran to completion but kopia wrote **no new manifest**: the
    /// source was byte-identical to the previous snapshot, and this policy has
    /// [`files.ignoreIdenticalSnapshots`](crate::snapshot_policy::Files::ignore_identical_snapshots)
    /// enabled.
    ///
    /// Terminal, and a **success**: the source was read and hashed, and it is
    /// protected — by the *previous* snapshot, which remains the live restore
    /// point. So an `Unchanged` run advances every liveness signal (last-backup
    /// timestamp, policy health, failure-streak reset) exactly like `Succeeded`.
    ///
    /// What it does NOT do is own a kopia manifest. `status.snapshot` is absent,
    /// the finalizer has nothing to reclaim, and it takes no GFS retention slot
    /// — a restore point that does not exist must not displace one that does.
    /// Recording it as `Succeeded` instead would make the controller resolve
    /// "its" snapshot and find its predecessor's, leaving two CRs claiming one
    /// manifest and the first prune deleting it out from under the second.
    ///
    /// Unreachable unless the policy opts in: the mover pins
    /// `--ignore-identical-snapshots=false` at the identity scope on every run.
    /// See #351.
    Unchanged,
    /// A phase string this build does not recognize — written by a newer
    /// operator during a rolling upgrade, or persisted before this variant set
    /// existed. Decode-compat only: hidden from the CRD schema (the apiserver
    /// rejects it on every new write) and never produced by this build.
    ///
    /// Never terminal, never schedulable, never reapable, never a success —
    /// every consumer holds and surfaces it rather than acting on a phase whose
    /// meaning it does not know.
    Unknown(String),
}

crate::common::phase_serde!(SnapshotPhase, "Lifecycle phase of a `Snapshot`.");

impl Origin {
    /// The stable wire/label value (the serde camelCase encoding), for the
    /// `kopiur.home-operations.com/origin` label and `status.origin` — single
    /// definition so producers (controller, kubectl plugin) cannot drift.
    pub fn label_value(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
            Self::Discovered => "discovered",
            Self::Adopted => "adopted",
        }
    }
}

/// Which operator lifecycle removed a Snapshot (the `pruned-by` annotation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrunedBy {
    /// GFS retention prune (`SnapshotPolicy.spec.retention`).
    Retention,
    /// `SnapshotSchedule.spec.failedJobsHistoryLimit` prune.
    FailedHistory,
    /// Policy-deletion cascade under `onPolicyDelete: Retain` — release the
    /// CR, never contact the repository.
    PolicyCascade,
}

impl PrunedBy {
    /// The stable annotation value stamped by the operator before it deletes a
    /// `Snapshot` as part of its own lifecycle (see [`crate::consts::PRUNED_BY_ANNOTATION`]).
    pub fn annotation_value(self) -> &'static str {
        match self {
            Self::Retention => "retention",
            Self::FailedHistory => "failed-history",
            Self::PolicyCascade => "policy-cascade",
        }
    }

    /// Strict parse: `None` for anything unrecognized (the finalizer must treat
    /// that as an EXTERNAL deletion — never guess "operator").
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "retention" => Some(Self::Retention),
            "failed-history" => Some(Self::FailedHistory),
            "policy-cascade" => Some(Self::PolicyCascade),
            _ => None,
        }
    }
}

impl SnapshotPhase {
    /// Whether this phase is **terminal**: the operator will do no further work
    /// on the object of its own accord, so a diagnostic must not report it as
    /// in-flight (nor as stuck).
    ///
    /// `Deleting` is deliberately **not** terminal. A CR sitting in `Deleting`
    /// has a finalizer that is still trying to reclaim its kopia snapshot — a
    /// wedged finalizer (an unreachable backend, a held mass-deletion breaker)
    /// is in-flight work that never completes, which is exactly the state worth
    /// surfacing. Classifying it terminal is how a stuck deletion becomes
    /// invisible.
    ///
    /// `Discovered` and `Unchanged` ARE terminal: a discovered CR mirrors a
    /// kopia snapshot the operator did not produce and never advances on its
    /// own, and an `Unchanged` run already finished (successfully, owning no
    /// manifest). A `Discovered` CR that is later deleted moves to `Deleting`
    /// like any other, so nothing is lost by treating the phase itself as done.
    ///
    /// Pure + exhaustive so the single definition lives in one tested place —
    /// the CLI and the controller must never disagree about what "still
    /// working" means.
    ///
    /// ```
    /// use kopiur_api::SnapshotPhase;
    ///
    /// assert!(SnapshotPhase::Succeeded.is_terminal());
    /// assert!(SnapshotPhase::Failed.is_terminal());
    /// assert!(SnapshotPhase::Discovered.is_terminal());
    /// assert!(SnapshotPhase::Unchanged.is_terminal());
    /// assert!(!SnapshotPhase::Pending.is_terminal());
    /// assert!(!SnapshotPhase::Running.is_terminal());
    /// // A wedged finalizer is in-flight work, not a finished object.
    /// assert!(!SnapshotPhase::Deleting.is_terminal());
    /// // An unrecognized phase is never terminal — hold and surface it.
    /// assert!(!SnapshotPhase::Unknown("Quiescing".into()).is_terminal());
    /// ```
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Succeeded | Self::Failed | Self::Discovered | Self::Unchanged => true,
            Self::Pending | Self::Running | Self::Deleting => false,
            // Conservative surface-it policy: a phase this build cannot
            // interpret must never be reported as finished work, or a newer
            // operator's in-flight (or wedged) object goes invisible to an
            // older CLI/reconciler. Not-terminal keeps it in every "still
            // working / worth looking at" set.
            Self::Unknown(_) => false,
        }
    }
}

impl crate::common::PhaseLabel for SnapshotPhase {
    const ALL: &'static [Self] = &[
        Self::Pending,
        Self::Running,
        Self::Succeeded,
        Self::Failed,
        Self::Deleting,
        Self::Discovered,
        Self::Unchanged,
    ];
    fn label(&self) -> &str {
        match self {
            Self::Pending => "Pending",
            Self::Running => "Running",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::Deleting => "Deleting",
            Self::Discovered => "Discovered",
            Self::Unchanged => "Unchanged",
            Self::Unknown(s) => s,
        }
    }
    fn unknown(raw: String) -> Self {
        Self::Unknown(raw)
    }
}

/// Observed state of a [`Snapshot`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotStatus {
    /// Current lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<SnapshotPhase>,
    /// Canonical origin (also mirrored to the `origin` label).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// `metadata.generation` last reconciled, for staleness detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// The kopia artifact this CR represents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotInfo>,
    /// Start/end/duration of the snapshot run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<SnapshotTiming>,
    /// Byte/file counts parsed from kopia's JSON output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<SnapshotStats>,
    /// The mover Job backing this run; absent for discovered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobStatus>,
    /// Frozen recipe values at run time (scheduled/manual).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedSnapshot>,
    /// Standard Kubernetes conditions (e.g. `SourcesQuiesced`, `SnapshotCreated`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// The last lines of the run's output, written by the mover at the terminal transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_tail: Option<String>,
    /// Structured terminal-failure detail (kopia error class, stderr tail, retry hint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<crate::common::FailureBlock>,
    /// The observed kopia-side pin state: `Some(true)` if pinned, `Some(false)` if unpinned, `None` before any pin reconcile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    /// Hook-execution bookkeeping so each hook list runs exactly once per Snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HookExecutionStatus>,
    /// The CSI staging objects the run created for `copyMethod: Snapshot`/`Clone`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<StagedSources>,
    /// RFC 3339 timestamp of the first reconcile where the repository was `Ready`
    /// but a `spec.preflight` check was failing. The one-shot anchor for the
    /// preflight `timeout` deadline (so the budget covers preflight only, not the
    /// earlier repository-not-Ready wait). Cleared once every preflight check passes,
    /// so a later failing episode gets a fresh budget rather than a stale anchor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight_since: Option<String>,
    /// Post-run cleanup bookkeeping, so each cleanup runs at most once per Snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<CleanupStatus>,
    /// The mover identity recorded on the kopia snapshot itself (the
    /// `kopiur-meta` tag): the resolved effective uid/gid/fsGroup the backup ran
    /// as, plus its provenance. Produced runs stamp this at launch (from the
    /// same value written into the tag); discovered rows decode it from the tag
    /// during the catalog scan. Absent for pre-feature snapshots, foreign
    /// backups without the tag, or a tag this operator version cannot decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded: Option<crate::recorded::RecordedSnapshotMeta>,
}

/// One-shot markers for the cleanups a terminal `Snapshot` performs, mirroring
/// [`HookExecutionStatus`]: the stamp IS the idempotence, so a stamped Snapshot's
/// steady-state reconcile is a no-op forever.
///
/// This is not just tidiness. A terminal `Snapshot` is re-reconciled every 10
/// minutes for the whole retention window (the steady-state requeue), and it is
/// retained as long as the kopia snapshot it owns — months. An ungated cleanup
/// probe would therefore re-issue its GETs against the apiserver, per Snapshot,
/// forever, to re-discover that there is nothing left to clean. `pin_job_may_exist`
/// exists in the same reconciler for exactly this reason.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CleanupStatus {
    /// When the run's projected credential Secrets were reclaimed (RFC 3339);
    /// absent until the reap has run. A projected copy is only needed while a mover
    /// Job can still load it via `envFrom`, but it is owner-ref'd to this CR, which
    /// long outlives that Job — so without an explicit reap it would sit in the
    /// workload namespace holding live repository credentials until the CR is pruned
    /// (#240).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creds_reaped_at: Option<String>,
}

/// The CSI staging objects a backup created so kopia reads a point-in-time copy of the source PVC.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StagedSources {
    /// Name of the shared CSI `VolumeGroupSnapshot` this member staged from,
    /// when the recipe asked for a consistency group
    /// ([`groupBy: VolumeGroupSnapshot`](crate::snapshot_policy::GroupBy)).
    ///
    /// Recorded because the group is otherwise invisible: it deliberately
    /// carries no ownerReferences (see `io::group_staging`), so this is how an
    /// operator tells which capture a backup came from — and how `kubectl
    /// kopiur doctor` finds one that outlived its members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_group_snapshot_name: Option<String>,
    /// The resolved capture method (`Snapshot` or `Clone`) that produced this stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_method: Option<String>,
    /// Name of the `VolumeSnapshot` created from the source PVC (`copyMethod: Snapshot` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_snapshot_name: Option<String>,
    /// Name of the staged `PersistentVolumeClaim` the mover mounts in place of the live source PVC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pvc_name: Option<String>,
    /// `true` once the stage is ready for the mover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<bool>,
    /// StorageClass of the staged PVC — `spec.staging.storageClassName` when set,
    /// else the source PVC's class. Pinned for observability (e.g. confirming a
    /// CephFS shallow-clone class actually took effect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    /// The resolved `spec.staging.timeout` (seconds) pinned when the stage was
    /// stamped, so the running-Job staged-PVC bind watchdog never re-resolves a
    /// policy that may have been edited or deleted mid-run. `0` = wait
    /// indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staging_timeout_seconds: Option<i64>,
}

/// When each hook list completed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HookExecutionStatus {
    /// When the `beforeSnapshot` list completed (RFC3339); absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_completed_at: Option<String>,
    /// When the `afterSnapshot` list completed (RFC3339); absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_completed_at: Option<String>,
}

/// Identifies the kopia snapshot a [`Snapshot`] CR owns.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInfo {
    /// kopia's snapshot ID — the handle the finalizer uses to delete content.
    #[serde(rename = "kopiaSnapshotID")]
    pub kopia_snapshot_id: String,
    /// The `username@hostname:path` identity recorded for this snapshot.
    pub identity: ResolvedIdentity,
    /// The kopia snapshot description (`snapshot create --description`), when
    /// one is recorded and non-empty. For discovered rows this is copied from
    /// the repository listing TRUNCATED to 1024 bytes (char-boundary-safe) —
    /// the value is foreign-writer-controlled and must never fail the CR write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Timing of a snapshot run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotTiming {
    /// RFC3339 start time of the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    /// RFC3339 end time of the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    /// Wall-clock duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<i64>,
}

/// Stats populated from kopia's JSON output.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotStats {
    /// Total logical size of the snapshot in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<i64>,
    /// Bytes newly uploaded this run (after dedup/compression).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_new: Option<i64>,
    /// Count of files new since the previous snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_new: Option<i64>,
    /// Count of files changed since the previous snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_modified: Option<i64>,
    /// Count of files unchanged since the previous snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_unchanged: Option<i64>,
    /// Count of source entries kopia could not read and excluded, making the snapshot incomplete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_failed: Option<i64>,
}

/// The mover Job backing a scheduled/manual `Snapshot`; absent for discovered.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    /// Name of the mover `Job`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Number of attempts so far (bounded by `failurePolicy.backoffLimit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<i32>,
}

/// Frozen recipe values pinned at run time.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedSnapshot {
    /// The repository this run targeted, frozen at run time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryRef>,
    /// The concrete PVCs + source paths backed up this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<ResolvedSource>,
    /// The recipe's `spec.credentialProjection` as it stood for this run.
    ///
    /// The deletion path re-projects the mover's credentials, but the opt-in lives on the
    /// `SnapshotPolicy` — which a user may delete first. Pinning it here lets the finalizer
    /// honor the opt-in that was actually in force, instead of reading an absent recipe as
    /// "projection off" and blocking on a Secret that was never meant to be namespace-local
    /// (#255). Absent only on a `Snapshot` that predates the pin or never ran; a run always
    /// writes it, including `enabled: false`, so absent stays distinguishable from off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_projection: Option<CredentialProjection>,
}

/// One resolved source backed up by a run — a concrete PVC and its kopia path.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedSource {
    /// `namespace/name` of the PVC, as kopia sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pvc: Option<String>,
    /// The source path kopia recorded for this PVC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// Derive the repository a `Snapshot` belongs to: a *produced* snapshot pins it
/// in `status.resolved.repository`; a *discovered* snapshot carries its
/// `Repository`/`ClusterRepository` as the controller `ownerReference` (it has
/// no `resolved` block). Pure. Shared by the `Restore` reconciler
/// (`spec.repository` derivation for `snapshotRef`) and the `kubectl kopiur`
/// browse data-plane, so the derivation rule cannot fork.
pub fn repository_ref_for(snap: &Snapshot) -> Option<RepositoryRef> {
    use crate::common::RepositoryKind;
    if let Some(rref) = snap
        .status
        .as_ref()
        .and_then(|s| s.resolved.as_ref())
        .and_then(|r| r.repository.clone())
    {
        return Some(rref);
    }
    let owners = snap
        .metadata
        .owner_references
        .as_deref()
        .unwrap_or_default();
    owners.iter().find_map(|o| {
        if o.api_version != crate::consts::API_VERSION {
            return None;
        }
        let kind = match o.kind.as_str() {
            "Repository" => RepositoryKind::Repository,
            "ClusterRepository" => RepositoryKind::ClusterRepository,
            _ => return None,
        };
        Some(RepositoryRef {
            kind,
            name: o.name.clone(),
            // Absent = resolved relative to the Snapshot's own namespace.
            namespace: None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::PhaseLabel;
    use crate::testutil::from_yaml;
    use kube::core::CustomResourceExt;

    #[test]
    fn origin_label_value_matches_the_serde_encoding() {
        for origin in [
            Origin::Scheduled,
            Origin::Manual,
            Origin::Discovered,
            Origin::Adopted,
        ] {
            assert_eq!(
                serde_json::to_value(origin).unwrap(),
                origin.label_value(),
                "{origin:?}"
            );
        }
    }

    #[test]
    fn backup_phase_all_covers_every_variant_uniquely() {
        // Guards the enumerate-and-reset contract: every variant is in ALL with
        // a unique, non-empty label. A new variant added without updating ALL
        // makes this fail (and `label`'s exhaustive match won't compile at all).
        let labels: Vec<&str> = SnapshotPhase::ALL.iter().map(|p| p.label()).collect();
        assert_eq!(SnapshotPhase::ALL.len(), 7);
        assert!(labels.iter().all(|l| !l.is_empty()));
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "phase labels must be unique");
        // Default is reachable through ALL.
        assert!(SnapshotPhase::ALL.contains(&SnapshotPhase::default()));
    }

    #[test]
    fn snapshot_terminal_set_is_pinned() {
        // Tripwire for the classifier every consumer (doctor, metrics, the
        // schedule's concurrency accounting) must agree on. Driven off ALL so a
        // NEW variant cannot join without a deliberate decision here — the
        // `is_terminal` match won't compile until it is classified, and this
        // assertion won't pass until the expected set is updated.
        let terminal: Vec<&str> = SnapshotPhase::ALL
            .iter()
            .filter(|p| p.is_terminal())
            .map(|p| p.label())
            .collect();
        assert_eq!(terminal, ["Succeeded", "Failed", "Discovered", "Unchanged"]);
        let in_flight: Vec<&str> = SnapshotPhase::ALL
            .iter()
            .filter(|p| !p.is_terminal())
            .map(|p| p.label())
            .collect();
        // `Deleting` stays here on purpose: a wedged finalizer is in-flight work.
        assert_eq!(in_flight, ["Pending", "Running", "Deleting"]);
    }

    /// Regression for the inert "derived from source" contract (found by the
    /// kubectl-plugin e2e): a snapshotRef Restore with no spec.repository was
    /// refused with "restore requires spec.repository" even though the CRD
    /// documents derivation. The pure derivation must cover both snapshot
    /// origins. (Moved here from the controller when the browse data-plane
    /// started sharing it.)
    mod repository_derivation {
        use super::super::repository_ref_for;
        use crate::Snapshot;
        use crate::common::RepositoryKind;

        fn snap(v: serde_json::Value) -> Snapshot {
            serde_json::from_value(v).expect("snapshot fixture")
        }

        #[test]
        fn produced_snapshot_uses_the_pinned_resolved_repository() {
            let s = snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": { "name": "s", "namespace": "media" },
                "spec": { "policyRef": { "name": "pol" } },
                "status": { "resolved": { "repository": { "kind": "ClusterRepository", "name": "nas" } } }
            }));
            let rref = repository_ref_for(&s).expect("derived");
            assert_eq!(rref.kind, RepositoryKind::ClusterRepository);
            assert_eq!(rref.name, "nas");
        }

        #[test]
        fn discovered_snapshot_uses_the_owning_repository() {
            for (kind_str, kind) in [
                ("Repository", RepositoryKind::Repository),
                ("ClusterRepository", RepositoryKind::ClusterRepository),
            ] {
                let s = snap(serde_json::json!({
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "Snapshot",
                    "metadata": {
                        "name": "repo-disc-abc", "namespace": "media",
                        "ownerReferences": [{
                            "apiVersion": "kopiur.home-operations.com/v1alpha1",
                            "kind": kind_str, "name": "nas", "uid": "u1", "controller": true
                        }]
                    },
                    "spec": {},
                    "status": { "phase": "Discovered", "origin": "discovered" }
                }));
                let rref = repository_ref_for(&s).expect(kind_str);
                assert_eq!(rref.kind, kind, "{kind_str}");
                assert_eq!(rref.name, "nas");
                assert_eq!(rref.namespace, None, "resolved relative to the snapshot ns");
            }
        }

        #[test]
        fn foreign_owners_and_bare_snapshots_derive_nothing() {
            // A non-kopiur owner (e.g. a Job) must not be mistaken for a repository.
            let s = snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": {
                    "name": "s", "namespace": "media",
                    "ownerReferences": [{
                        "apiVersion": "batch/v1", "kind": "Job", "name": "j", "uid": "u2"
                    }]
                },
                "spec": {}
            }));
            assert!(repository_ref_for(&s).is_none());
        }
    }

    #[test]
    fn backup_crd_metadata_is_correct() {
        let crd = Snapshot::crd();
        assert_eq!(crd.spec.group, "kopiur.home-operations.com");
        assert_eq!(crd.spec.names.kind, "Snapshot");
        assert_eq!(crd.spec.scope, "Namespaced");
        assert_eq!(crd.spec.versions[0].name, "v1alpha1");
    }

    #[test]
    fn backup_manual_roundtrip_matches_adr_shape() {
        // Mirrors ADR-0001 §3.4 spec block + §5.6.
        let yaml = r#"
policyRef: { name: postgres-data }
tags:
  reason: "scheduled-nightly"
failurePolicy:
  backoffLimit: 2
  activeDeadlineSeconds: 7200
deletionPolicy: Delete
"#;
        let spec: SnapshotSpec = from_yaml(yaml);
        assert_eq!(spec.policy_ref.as_ref().unwrap().name, "postgres-data");
        assert_eq!(spec.tags.as_ref().unwrap()["reason"], "scheduled-nightly");
        assert_eq!(spec.failure_policy.as_ref().unwrap().backoff_limit, Some(2));
        assert_eq!(spec.deletion_policy, Some(DeletionPolicy::Delete));

        let json = serde_json::to_value(&spec).expect("serialize");
        let reparsed: SnapshotSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn backup_discovered_spec_is_empty() {
        // Discovered backups carry no spec fields.
        let spec: SnapshotSpec = from_yaml("{}\n");
        assert!(spec.policy_ref.is_none());
        assert!(spec.deletion_policy.is_none());
        // Empty spec serializes to an empty object (all fields skip).
        assert_eq!(serde_json::to_value(&spec).unwrap(), serde_json::json!({}));
    }

    #[test]
    fn deletion_policy_serializes_to_expected_strings() {
        assert_eq!(
            serde_json::to_value(DeletionPolicy::Delete).unwrap(),
            "Delete"
        );
        assert_eq!(
            serde_json::to_value(DeletionPolicy::Retain).unwrap(),
            "Retain"
        );
        assert_eq!(
            serde_json::to_value(DeletionPolicy::Orphan).unwrap(),
            "Orphan"
        );
        // DeletionPolicy is Copy (ADR-0003 §4.5).
        let p = DeletionPolicy::Retain;
        let _copy = p;
        assert_eq!(p, DeletionPolicy::Retain);
    }

    #[test]
    fn on_schedule_delete_round_trips_and_absent_stays_absent() {
        let yaml = r#"
policyRef: { name: postgres-data }
deletionPolicy: Delete
onScheduleDelete: Delete
"#;
        let spec: SnapshotSpec = from_yaml(yaml);
        assert_eq!(spec.on_schedule_delete, Some(ScheduleDeletePolicy::Delete));
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["onScheduleDelete"], "Delete");
        let reparsed: SnapshotSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Absent stays absent (no schema default — the safety default lives in
        // the controller resolver, not here).
        let bare: SnapshotSpec = from_yaml("policyRef: { name: postgres-data }\n");
        assert!(bare.on_schedule_delete.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("onScheduleDelete")
                .is_none(),
            "absent onScheduleDelete must be elided"
        );
    }

    #[test]
    fn pruned_by_parse_is_the_exact_inverse_of_annotation_value() {
        for variant in [
            PrunedBy::Retention,
            PrunedBy::FailedHistory,
            PrunedBy::PolicyCascade,
        ] {
            assert_eq!(
                PrunedBy::parse(variant.annotation_value()),
                Some(variant),
                "{variant:?}"
            );
        }
        assert_eq!(PrunedBy::parse("garbage"), None);
    }

    #[test]
    fn origin_and_phase_serialize_to_expected_strings() {
        assert_eq!(
            serde_json::to_value(Origin::Scheduled).unwrap(),
            "scheduled"
        );
        assert_eq!(serde_json::to_value(Origin::Manual).unwrap(), "manual");
        assert_eq!(
            serde_json::to_value(Origin::Discovered).unwrap(),
            "discovered"
        );
        assert_eq!(serde_json::to_value(Origin::Adopted).unwrap(), "adopted");
        assert_eq!(
            serde_json::to_value(SnapshotPhase::Succeeded).unwrap(),
            "Succeeded"
        );
        assert_eq!(
            serde_json::to_value(SnapshotPhase::Deleting).unwrap(),
            "Deleting"
        );
    }

    #[test]
    fn backup_status_roundtrips() {
        // Mirrors ADR-0001 §3.4 status block.
        let yaml = r#"
phase: Succeeded
origin: scheduled
snapshot:
  kopiaSnapshotID: k1f1ec0a8
  identity:
    username: postgres-data
    hostname: billing
    sourcePath: /data
timing:
  startTime: 2026-05-24T02:13:00Z
  endTime: 2026-05-24T02:18:42Z
  durationSeconds: 342
stats:
  sizeBytes: 4321098765
  bytesNew: 12345678
  filesNew: 1233
resolved:
  repository: { kind: Repository, name: nas-primary, namespace: backups }
  sources:
    - pvc: billing/postgres-data
      sourcePath: /data
logTail: "Snapshot created: k1f1ec0a8"
"#;
        let status: SnapshotStatus = from_yaml(yaml);
        assert_eq!(status.phase, Some(SnapshotPhase::Succeeded));
        assert_eq!(status.origin, Some(Origin::Scheduled));
        assert_eq!(
            status.snapshot.as_ref().unwrap().kopia_snapshot_id,
            "k1f1ec0a8"
        );
        assert_eq!(status.stats.as_ref().unwrap().size_bytes, Some(4321098765));

        let json = serde_json::to_value(&status).unwrap();
        let reparsed: SnapshotStatus = serde_json::from_value(json).unwrap();
        assert_eq!(status, reparsed);
    }

    #[test]
    fn backup_status_recorded_and_description_roundtrip() {
        use crate::recorded::{RecordedSnapshotMeta, RecordedSrc};
        let yaml = r#"
phase: Succeeded
snapshot:
  kopiaSnapshotID: k1
  identity:
    username: u
    hostname: h
    sourcePath: /data
  description: "pre-upgrade snapshot"
recorded:
  schema: 1
  src: inherited
  uid: 3001
  gid: 3001
  fsGroup: 65532
"#;
        let status: SnapshotStatus = from_yaml(yaml);
        assert_eq!(
            status.recorded,
            Some(RecordedSnapshotMeta {
                schema: 1,
                src: RecordedSrc::Inherited,
                uid: Some(3001),
                gid: Some(3001),
                fs_group: Some(65532),
            })
        );
        assert_eq!(
            status.snapshot.as_ref().unwrap().description.as_deref(),
            Some("pre-upgrade snapshot")
        );
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["recorded"]["fsGroup"], 65532, "camelCase wire key");
        let reparsed: SnapshotStatus = serde_json::from_value(json).unwrap();
        assert_eq!(status, reparsed);

        // Absent stays absent — no null/{} noise on old rows.
        let bare: SnapshotStatus = from_yaml("phase: Succeeded\n");
        assert!(bare.recorded.is_none());
        let wire = serde_json::to_value(&bare).unwrap();
        assert!(wire.get("recorded").is_none());
    }

    #[test]
    fn stored_recorded_with_future_src_decodes_gracefully() {
        // A newer operator wrote `src: workload` onto status; this version's
        // typed watcher must decode it (graceful-decode convention), not error.
        use crate::recorded::RecordedSrc;
        let status: SnapshotStatus =
            from_yaml("recorded:\n  schema: 1\n  src: workload\n  uid: 7\n");
        let rec = status.recorded.expect("decoded");
        assert_eq!(rec.src, RecordedSrc::Unknown);
        assert_eq!(rec.uid, Some(7));
    }
}
