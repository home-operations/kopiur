//! The `Snapshot` CRD — a single kopia snapshot as a Kubernetes object.
//! ADR-0001 §3.4, ADR-0003 §4.5.
//!
//! Origins (canonical value lives in `status.origin`):
//! - `scheduled` — created by a `SnapshotSchedule`; spec carries `policyRef`.
//! - `manual`    — created by `kubectl create` / external automation; spec carries `policyRef`.
//! - `discovered`— materialized by the catalog scan; spec is empty/absent.
//! - `adopted`   — a discovered row re-attached to a live `SnapshotPolicy`.
//! - `replicated`— a dest-side copy CR minted by a `SnapshotReplication` run;
//!   spec carries `repository` (the destination pin) and no `policyRef`.

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
    /// The ONE repository this `Snapshot` targets, pinned by value at mint
    /// time. Stamped by a multi-repository `SnapshotPolicy` fan-out (each child
    /// covers exactly one member of the policy's repository set) and by
    /// `SnapshotReplication` copy CRs (the destination repository). Absent for
    /// the legacy single-repository case, where the policy's own
    /// `spec.repository` (or, for catalog rows, the owning repository CR) is
    /// the answer — an absent pin resolves exactly as before this field
    /// existed, so pre-feature `Snapshot`s are untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryRef>,
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
    /// A destination-side copy CR minted by a `SnapshotReplication` run: the
    /// kopia snapshot it represents was `snapshot migrate`d from another
    /// repository, not produced by a backup run here. Like `discovered`/
    /// `adopted` it is catalog history — it must never enter the backup-run
    /// machinery — but it is pruned only by its `SnapshotReplication`'s own
    /// pruning mode, never by any policy's GFS retention.
    Replicated,
}

impl Origin {
    /// Every variant, for the label-value ↔ [`parse`](Self::parse) round-trip
    /// tests and consumer-side variant-count guards (e.g. the CLI's
    /// `OriginFilter`). A new variant added without extending this array fails
    /// the round-trip test (and `label_value`/`parse`'s exhaustive matches
    /// won't compile until it is classified).
    pub const ALL: &'static [Self] = &[
        Self::Scheduled,
        Self::Manual,
        Self::Discovered,
        Self::Adopted,
        Self::Replicated,
    ];
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
            Self::Replicated => "replicated",
        }
    }

    /// Strict, TOTAL parse of an origin marker (the `origin` label /
    /// `status.origin` wire value): `None` for anything unrecognized.
    ///
    /// The single inverse of [`label_value`](Self::label_value), so string
    /// matchers (the controller's `resolve_origin`, the webhook's
    /// `backup_origin`) can never silently classify an unknown origin as a
    /// known one. The pre-parse versions of both defaulted unknown strings to
    /// `Manual` — which would have routed a row written by a NEWER operator
    /// (e.g. `replicated` before this variant existed) into the backup-run
    /// machinery and minted a mover Job for a snapshot this build does not
    /// understand. Callers must treat `None` conservatively: warn + inert
    /// handling, never `Manual`.
    pub fn parse(v: &str) -> Option<Self> {
        // One arm per variant (each also pinned by the ALL round-trip test);
        // the trailing arm is the UNKNOWN-string case, not a variant catch-all.
        match v {
            "scheduled" => Some(Self::Scheduled),
            "manual" => Some(Self::Manual),
            "discovered" => Some(Self::Discovered),
            "adopted" => Some(Self::Adopted),
            "replicated" => Some(Self::Replicated),
            _ => None,
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
    /// A `SnapshotReplication`'s own retention prune of its dest-side copy CRs
    /// (`pruning: retention`). An OPERATOR prune exactly like `Retention`:
    /// bounded, deliberate, breaker-exempt. (`pruning: mirrorSource` deletes
    /// deliberately carry NO stamp, so a mass source-vanish classifies EXTERNAL
    /// and the dest repository's breaker holds it.)
    ReplicationRetention,
    /// `SnapshotSchedule.spec.schedule.concurrencyPolicy: Replace` cancelled
    /// this still-unfinished run so the newly-due slot could take its place.
    /// An OPERATOR prune: bounded by construction (at most this schedule's own
    /// unfinished children, at most one slot's worth per fire) and deliberate —
    /// the user asked for cancel-the-old — so it is breaker-EXEMPT. Without the
    /// stamp every `Replace` fire would classify EXTERNAL and a busy schedule
    /// would trip its repository's mass-deletion breaker.
    ReplacedRun,
}

impl PrunedBy {
    /// Every variant, for the annotation-value ↔ [`parse`](Self::parse)
    /// round-trip test. A new variant added without extending this array fails
    /// that test (and the exhaustive matches here and in the controller's
    /// deletion planner won't compile until it is classified).
    pub const ALL: &'static [Self] = &[
        Self::Retention,
        Self::FailedHistory,
        Self::PolicyCascade,
        Self::ReplicationRetention,
        Self::ReplacedRun,
    ];

    /// The stable annotation value stamped by the operator before it deletes a
    /// `Snapshot` as part of its own lifecycle (see [`crate::consts::PRUNED_BY_ANNOTATION`]).
    pub fn annotation_value(self) -> &'static str {
        match self {
            Self::Retention => "retention",
            Self::FailedHistory => "failed-history",
            Self::PolicyCascade => "policy-cascade",
            Self::ReplicationRetention => "replication-retention",
            Self::ReplacedRun => "replaced-run",
        }
    }

    /// Strict parse: `None` for anything unrecognized (the finalizer must treat
    /// that as an EXTERNAL deletion — never guess "operator").
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "retention" => Some(Self::Retention),
            "failed-history" => Some(Self::FailedHistory),
            "policy-cascade" => Some(Self::PolicyCascade),
            "replication-retention" => Some(Self::ReplicationRetention),
            "replaced-run" => Some(Self::ReplacedRun),
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

    /// Whether this phase is the **decode sentinel** — a value the running build
    /// cannot interpret, kept verbatim by [`Unknown`](Self::Unknown) instead of
    /// failing the whole typed `list()`/watch (#359, defect 3).
    ///
    /// The contract is narrow on purpose, and it is the reason this is a method
    /// rather than an inline `matches!` at each caller: `true` means *only*
    /// "this string is not a phase this binary knows", never "unusual" or
    /// "not one I handle". A canonical variant added to this enum later is by
    /// definition **not** the sentinel, so `false` is the right answer for it —
    /// which is exactly why the exhaustive `match` below is written out. Callers
    /// asking a set-shaped question ("is this finished?", "is this a failure?")
    /// want [`is_terminal`](Self::is_terminal) or their own exhaustive match,
    /// not this.
    ///
    /// ```
    /// use kopiur_api::SnapshotPhase;
    ///
    /// assert!(SnapshotPhase::Unknown("Quiescing".into()).is_unknown());
    /// assert!(!SnapshotPhase::Succeeded.is_unknown());
    /// assert!(!SnapshotPhase::Failed.is_unknown());
    /// assert!(!SnapshotPhase::Deleting.is_unknown());
    /// ```
    pub fn is_unknown(&self) -> bool {
        match self {
            Self::Unknown(_) => true,
            Self::Pending
            | Self::Running
            | Self::Succeeded
            | Self::Failed
            | Self::Deleting
            | Self::Discovered
            | Self::Unchanged => false,
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
    /// Lineage for `origin: replicated` rows: the source repository, source
    /// manifest id, and `startTime` the copy was migrated from. Written by the
    /// `SnapshotReplication` mover in the same atomic PATCH as
    /// `status.snapshot`; absent on every other origin. See [`CopiedFrom`] for
    /// why this lives on the CR rather than as kopia tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copied_from: Option<CopiedFrom>,
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

/// Lineage of an `origin: replicated` row: where the copy came from.
///
/// `kopia snapshot migrate` cannot stamp tags onto the migrated manifest (it
/// preserves the source manifest verbatim apart from assigning a new manifest
/// id), so the provenance a dest-side copy CR needs — which repository it was
/// copied FROM, which source manifest it corresponds to, and the `startTime`
/// migrate keys idempotency on — cannot live in the kopia repository. It lives
/// here, written by the replication mover in the same atomic status PATCH that
/// records the destination manifest (`status.snapshot.kopiaSnapshotID`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CopiedFrom {
    /// The SOURCE repository the snapshot was migrated from (the
    /// `SnapshotReplication`'s `sourceRef`, resolved at run time).
    pub repository: RepositoryRef,
    /// The kopia manifest id the snapshot had in the SOURCE repository.
    /// Migrate assigns a NEW manifest id on the destination
    /// (`status.snapshot.kopiaSnapshotID`); this is the old one, kept for
    /// cross-repository correlation.
    pub source_manifest_id: String,
    /// The snapshot's RFC3339 `startTime` — preserved verbatim by migrate and
    /// the key (together with the identity triple) both idempotent re-migration
    /// and `pruning: mirrorSource` correlate source and destination rows on.
    pub start_time: String,
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

/// Derive the repository a `Snapshot` belongs to, in fixed precedence order:
///
/// 1. `status.resolved.repository` — the run-time pin every *produced*
///    snapshot records (controller-written, authoritative);
/// 2. `spec.repository` — the mint-time pin a multi-repo policy fan-out child
///    or a `SnapshotReplication` copy CR carries (present before any status
///    has been written, and the only pin such a CR is guaranteed to have);
/// 3. the `Repository`/`ClusterRepository` controller `ownerReference` a
///    *discovered* snapshot carries (it has neither block).
///
/// Pure. Shared by the `Restore` reconciler (`spec.repository` derivation for
/// `snapshotRef`) and the `kubectl kopiur` browse data-plane, so the
/// derivation rule cannot fork.
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
    if let Some(rref) = snap.spec.repository.clone() {
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

/// THE repository a policy-child `Snapshot` runs against, resolved from the
/// mint-time pin + the referenced policy's repository set. **Pure.** This is
/// the single decision every launch/deletion/pin/preflight path shares, so
/// multi-repo pin semantics cannot fork between consumers.
///
/// The rule, in order:
///
/// 1. **No `policyRef`** — this is NOT a policy child (a `SnapshotReplication`
///    copy CR, or a discovered row): the `policy` argument is not this row's
///    recipe and is ignored; the row's own derivation
///    ([`repository_ref_for`]: status pin → spec pin → owner ref) answers, or
///    [`ValidationError::SnapshotRepositoryUnresolvable`] when it has none.
/// 2. **Pin present** (`spec.repository`) — the pin wins, but it must still be
///    a member of the policy's CURRENT repository set (compared by normalized
///    [`repo_key`](crate::common::repo_key) against `policy_ns`); a pin the
///    recipe no longer lists is the terminal
///    [`ValidationError::SnapshotPinNotInPolicy`] ("the recipe was edited out
///    from under this Snapshot's pin"), never a silent re-target.
/// 3. **No pin, single-repo policy** — the policy's one `spec.repository`,
///    verbatim (byte-identical to the pre-multi-repo behavior).
/// 4. **No pin, multi-repo policy** —
///    [`ValidationError::MultiRepoSnapshotUnpinned`]: the controller-side
///    backstop of the admission rule; repository #1 is never guessed.
///
/// A malformed policy (neither/both repository shapes) surfaces as its
/// [`ValidationError::PolicyRepositoryExactlyOne`].
pub fn effective_repository_ref(
    snap: &Snapshot,
    policy: &crate::snapshot_policy::SnapshotPolicySpec,
    policy_ns: &str,
) -> Result<RepositoryRef, crate::error::ValidationError> {
    use crate::common::repo_key;
    use crate::snapshot_policy::{PolicyRepositories, policy_repositories};
    use kube::ResourceExt;

    if snap.spec.policy_ref.is_none() {
        return repository_ref_for(snap).ok_or_else(|| {
            crate::error::ValidationError::SnapshotRepositoryUnresolvable {
                snapshot: snap.name_any(),
            }
        });
    }
    let repos = policy_repositories(policy)?;
    if let Some(pin) = snap.spec.repository.as_ref() {
        // The pin was stamped NORMALIZED at mint, so keying it against the
        // policy's namespace is stable; the members normalize against the same
        // namespace the policy resolves them in.
        let pin_key = repo_key(pin, policy_ns);
        let members: Vec<&RepositoryRef> = match repos {
            PolicyRepositories::Single(r) => vec![r],
            PolicyRepositories::Multi(rs) => rs.iter().collect(),
        };
        return match members.iter().find(|m| repo_key(m, policy_ns) == pin_key) {
            Some(_) => Ok(pin.clone()),
            None => Err(crate::error::ValidationError::SnapshotPinNotInPolicy {
                pin: pin_key,
                policy: snap
                    .spec
                    .policy_ref
                    .as_ref()
                    .map(|p| p.name.clone())
                    .unwrap_or_default(),
                valid: members
                    .iter()
                    .map(|m| repo_key(m, policy_ns))
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
        };
    }
    match repos {
        PolicyRepositories::Single(r) => Ok(r.clone()),
        PolicyRepositories::Multi(_) => {
            Err(crate::error::ValidationError::MultiRepoSnapshotUnpinned {
                policy: snap
                    .spec
                    .policy_ref
                    .as_ref()
                    .map(|p| p.name.clone())
                    .unwrap_or_default(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::PhaseLabel;
    use crate::testutil::from_yaml;
    use kube::core::CustomResourceExt;

    #[test]
    fn origin_label_value_matches_the_serde_encoding() {
        // Hard-coded ALL-variants array: adding a variant without classifying
        // it here fails the length check, and the serde/label/parse trio must
        // agree byte-for-byte for every variant.
        let all = [
            Origin::Scheduled,
            Origin::Manual,
            Origin::Discovered,
            Origin::Adopted,
            Origin::Replicated,
        ];
        assert_eq!(Origin::ALL, all, "Origin::ALL must list every variant");
        for origin in all {
            assert_eq!(
                serde_json::to_value(origin).unwrap(),
                origin.label_value(),
                "{origin:?}"
            );
        }
    }

    #[test]
    fn origin_parse_is_the_exact_inverse_of_label_value() {
        for origin in [
            Origin::Scheduled,
            Origin::Manual,
            Origin::Discovered,
            Origin::Adopted,
            Origin::Replicated,
        ] {
            assert_eq!(
                Origin::parse(origin.label_value()),
                Some(origin),
                "{origin:?}"
            );
        }
        // Unknown strings never resolve — in particular never to Manual, the
        // pre-parse default that would mint a backup Job for a foreign row.
        for garbage in ["", "Manual", "SCHEDULED", "replicated ", "garbage"] {
            assert_eq!(Origin::parse(garbage), None, "{garbage:?}");
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

    /// The 5-way contract of [`effective_repository_ref`] — the single
    /// launch/deletion/pin/preflight repository decision (multi-repo fan-out,
    /// #368). Specs are constructed directly, the same shapes admission
    /// accepts now that the M7 feature gate is lifted.
    mod effective_repository {
        use super::super::effective_repository_ref;
        use crate::error::ValidationError;
        use crate::{Snapshot, SnapshotPolicySpec};

        fn snap(v: serde_json::Value) -> Snapshot {
            serde_json::from_value(v).expect("snapshot fixture")
        }

        fn policy(v: serde_json::Value) -> SnapshotPolicySpec {
            serde_json::from_value(v).expect("policy fixture")
        }

        fn multi_policy() -> SnapshotPolicySpec {
            policy(serde_json::json!({
                "repositories": [
                    { "kind": "Repository", "name": "a" },
                    { "kind": "ClusterRepository", "name": "b" },
                ],
                "sources": [ { "pvc": { "name": "d" } } ],
            }))
        }

        #[test]
        fn no_policy_ref_short_circuits_to_the_rows_own_derivation() {
            // A replication copy CR: no policyRef, spec pin present. The policy
            // argument (a multi-repo recipe that does NOT list the pin) is
            // ignored — it is not this row's recipe.
            let s = snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": { "name": "copy", "namespace": "media" },
                "spec": { "repository": { "kind": "ClusterRepository", "name": "offsite" } }
            }));
            let r = effective_repository_ref(&s, &multi_policy(), "media").unwrap();
            assert_eq!(r.name, "offsite");

            // …and a bare row with nothing to derive from is a NAMED error,
            // never a guess.
            let bare = snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": { "name": "bare", "namespace": "media" },
                "spec": {}
            }));
            assert!(matches!(
                effective_repository_ref(&bare, &multi_policy(), "media").unwrap_err(),
                ValidationError::SnapshotRepositoryUnresolvable { snapshot } if snapshot == "bare"
            ));
        }

        #[test]
        fn pin_that_is_a_member_wins() {
            let s = snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": { "name": "s", "namespace": "media" },
                "spec": {
                    "policyRef": { "name": "pol" },
                    // Normalized pin: the Repository member resolves to
                    // media/a, and so does this explicit-namespace pin.
                    "repository": { "kind": "Repository", "name": "a", "namespace": "media" }
                }
            }));
            let r = effective_repository_ref(&s, &multi_policy(), "media").unwrap();
            assert_eq!(r.name, "a");
            assert_eq!(r.namespace.as_deref(), Some("media"));
        }

        #[test]
        fn pin_edited_out_of_the_policy_is_terminal() {
            let s = snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": { "name": "s", "namespace": "media" },
                "spec": {
                    "policyRef": { "name": "pol" },
                    "repository": { "kind": "Repository", "name": "gone", "namespace": "media" }
                }
            }));
            let err = effective_repository_ref(&s, &multi_policy(), "media").unwrap_err();
            match err {
                ValidationError::SnapshotPinNotInPolicy { pin, policy, valid } => {
                    assert_eq!(pin, "Repository/media/gone");
                    assert_eq!(policy, "pol");
                    assert_eq!(valid, "Repository/media/a, ClusterRepository/b");
                }
                other => panic!("expected SnapshotPinNotInPolicy, got {other:?}"),
            }
        }

        #[test]
        fn unpinned_single_repo_child_uses_the_single_ref() {
            let single = policy(serde_json::json!({
                "repository": { "kind": "Repository", "name": "r" },
                "sources": [ { "pvc": { "name": "d" } } ],
            }));
            let s = snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": { "name": "s", "namespace": "media" },
                "spec": { "policyRef": { "name": "pol" } }
            }));
            // Verbatim — byte-identical to the pre-multi-repo behavior (no
            // namespace materialized that the spec didn't carry).
            let r = effective_repository_ref(&s, &single, "media").unwrap();
            assert_eq!(r.name, "r");
            assert_eq!(r.namespace, None);
        }

        #[test]
        fn unpinned_multi_repo_child_is_refused_never_guessed() {
            let s = snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": { "name": "s", "namespace": "media" },
                "spec": { "policyRef": { "name": "pol" } }
            }));
            assert!(matches!(
                effective_repository_ref(&s, &multi_policy(), "media").unwrap_err(),
                ValidationError::MultiRepoSnapshotUnpinned { policy } if policy == "pol"
            ));
        }

        #[test]
        fn pin_against_a_single_repo_policy_still_checks_membership() {
            // A multi→single edit that removed the pinned repo is the same
            // "edited out from under the pin" terminal error, not a silent
            // re-target onto the surviving repository.
            let single = policy(serde_json::json!({
                "repository": { "kind": "Repository", "name": "kept" },
                "sources": [ { "pvc": { "name": "d" } } ],
            }));
            let s = snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": { "name": "s", "namespace": "media" },
                "spec": {
                    "policyRef": { "name": "pol" },
                    "repository": { "kind": "Repository", "name": "removed", "namespace": "media" }
                }
            }));
            assert!(matches!(
                effective_repository_ref(&s, &single, "media").unwrap_err(),
                ValidationError::SnapshotPinNotInPolicy { .. }
            ));
            // …while a pin that matches the single ref proceeds.
            let matching = snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": { "name": "s", "namespace": "media" },
                "spec": {
                    "policyRef": { "name": "pol" },
                    "repository": { "kind": "Repository", "name": "kept", "namespace": "media" }
                }
            }));
            assert_eq!(
                effective_repository_ref(&matching, &single, "media")
                    .unwrap()
                    .name,
                "kept"
            );
        }
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
        fn spec_pin_wins_over_owner_ref_but_loses_to_status() {
            // Precedence: status.resolved.repository → spec.repository → ownerRef.
            // A replication copy CR (or multi-repo fan-out child) carries the
            // spec pin from CREATE, before any status exists…
            let spec_only = snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": {
                    "name": "s", "namespace": "media",
                    // A repository ownerRef that must NOT win over the spec pin.
                    "ownerReferences": [{
                        "apiVersion": "kopiur.home-operations.com/v1alpha1",
                        "kind": "Repository", "name": "owner-repo", "uid": "u1"
                    }]
                },
                "spec": { "repository": { "kind": "ClusterRepository", "name": "offsite" } }
            }));
            let rref = repository_ref_for(&spec_only).expect("derived from spec");
            assert_eq!(rref.kind, RepositoryKind::ClusterRepository);
            assert_eq!(rref.name, "offsite");

            // …and once the run-time status pin lands, it stays authoritative.
            let with_status = snap(serde_json::json!({
                "apiVersion": "kopiur.home-operations.com/v1alpha1",
                "kind": "Snapshot",
                "metadata": { "name": "s", "namespace": "media" },
                "spec": { "repository": { "kind": "ClusterRepository", "name": "offsite" } },
                "status": { "resolved": { "repository": { "kind": "Repository", "name": "nas" } } }
            }));
            let rref = repository_ref_for(&with_status).expect("derived from status");
            assert_eq!(rref.kind, RepositoryKind::Repository);
            assert_eq!(rref.name, "nas");
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
        assert!(spec.repository.is_none());
        // Empty spec serializes to an empty object (all fields skip).
        assert_eq!(serde_json::to_value(&spec).unwrap(), serde_json::json!({}));
    }

    #[test]
    fn backup_spec_repository_pin_round_trips_and_absent_stays_absent() {
        use crate::common::RepositoryKind;
        // The mint-time pin a multi-repo fan-out child / replication copy CR
        // carries (nothing stamps it yet — the field is decode/encode-ready).
        let spec: SnapshotSpec =
            from_yaml("repository: { kind: ClusterRepository, name: offsite }\n");
        let pin = spec.repository.as_ref().expect("pin decoded");
        assert_eq!(pin.kind, RepositoryKind::ClusterRepository);
        assert_eq!(pin.name, "offsite");
        let json = serde_json::to_value(&spec).expect("serialize");
        assert_eq!(json["repository"]["name"], "offsite");
        let reparsed: SnapshotSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);

        // Legacy single-repo children stay byte-identical: absent is elided.
        let bare: SnapshotSpec = from_yaml("policyRef: { name: postgres-data }\n");
        assert!(bare.repository.is_none());
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("repository")
                .is_none(),
            "absent repository pin must be elided"
        );
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
        let all = [
            PrunedBy::Retention,
            PrunedBy::FailedHistory,
            PrunedBy::PolicyCascade,
            PrunedBy::ReplicationRetention,
            PrunedBy::ReplacedRun,
        ];
        assert_eq!(PrunedBy::ALL, all, "PrunedBy::ALL must list every variant");
        // The `concurrencyPolicy: Replace` stamp, pinned as a literal: it is a
        // wire value the finalizer parses, so a rename would silently reclassify
        // every replaced run as an EXTERNAL deletion and push the repository's
        // mass-deletion breaker toward tripping on every busy schedule.
        assert_eq!(PrunedBy::ReplacedRun.annotation_value(), "replaced-run");
        assert_eq!(PrunedBy::parse("replaced-run"), Some(PrunedBy::ReplacedRun));
        assert_eq!(PrunedBy::parse("replaced_run"), None);
        for variant in all {
            assert_eq!(
                PrunedBy::parse(variant.annotation_value()),
                Some(variant),
                "{variant:?}"
            );
        }
        // Unrecognized ⇒ None ⇒ the finalizer classifies the deletion EXTERNAL
        // (breaker-relevant) — a missed parse arm here would silently invert a
        // new operator prune into a breaker-held external wave.
        assert_eq!(PrunedBy::parse("garbage"), None);
        assert_eq!(PrunedBy::parse("replication_retention"), None);
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
            serde_json::to_value(Origin::Replicated).unwrap(),
            "replicated"
        );
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
    fn replicated_status_copied_from_roundtrips_and_absent_stays_absent() {
        use crate::common::RepositoryKind;
        // The lineage block a SnapshotReplication copy CR carries (migrate
        // cannot stamp kopia tags, so provenance lives on the CR status).
        let yaml = r#"
phase: Succeeded
origin: replicated
snapshot:
  kopiaSnapshotID: destid123
  identity:
    username: mydb
    hostname: prod
    sourcePath: /pvc/mydb
copiedFrom:
  repository: { kind: Repository, name: nas-primary, namespace: backups }
  sourceManifestId: srcid456
  startTime: 2026-08-01T02:00:00Z
"#;
        let status: SnapshotStatus = from_yaml(yaml);
        let cf = status.copied_from.as_ref().expect("copiedFrom decoded");
        assert_eq!(cf.repository.kind, RepositoryKind::Repository);
        assert_eq!(cf.repository.name, "nas-primary");
        assert_eq!(cf.source_manifest_id, "srcid456");
        assert_eq!(cf.start_time, "2026-08-01T02:00:00Z");

        // The exact camelCase wire keys — a drifting name is silently pruned
        // by the apiserver's structural schema.
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["copiedFrom"]["sourceManifestId"], "srcid456");
        assert_eq!(json["copiedFrom"]["startTime"], "2026-08-01T02:00:00Z");
        assert_eq!(json["copiedFrom"]["repository"]["name"], "nas-primary");
        let reparsed: SnapshotStatus = serde_json::from_value(json).unwrap();
        assert_eq!(status, reparsed);

        // Every other origin: absent stays absent (no null/{} noise).
        let bare: SnapshotStatus = from_yaml("phase: Succeeded\n");
        assert!(bare.copied_from.is_none());
        let wire = serde_json::to_value(&bare).unwrap();
        assert!(wire.get("copiedFrom").is_none());
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
